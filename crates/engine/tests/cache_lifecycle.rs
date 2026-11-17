//! Cache budget, pins, damage repair, disk-full fallback, and merge admission
//! against a real engine: eviction never changes an answer, and a merge never
//! starts without room.
#[path = "support/remote_fixture.rs"]
mod fixture;

use fixture::{Scratch, answers, schema, write_documents};
use hsearch_engine::{
    CachedDirectory, DiskBudget, Engine, EngineDirectory, Error, MergeScheduler, Mutation,
    RemoteError, RemoteFiles, Snapshot, Value,
};
use lucene_store::{Directory, FsDirectory};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Default)]
struct Store {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
}
impl RemoteFiles for &Store {
    fn fetch(&self, name: &str) -> Result<Vec<u8>, RemoteError> {
        self.files
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or(RemoteError::NotFound)
    }
}

fn uploaded() -> (Scratch, Store, Vec<String>) {
    let store = Store::default();
    let (scratch, files) = write_documents();
    let mut names = Vec::new();
    for (name, bytes) in files {
        store.files.lock().unwrap().insert(name.clone(), bytes);
        names.push(name);
    }
    (scratch, store, names)
}

#[test]
fn a_pinned_file_survives_eviction_pressure_and_live_snapshots_keep_answering() {
    let (written, store, names) = uploaded();
    let local = EngineDirectory::new(FsDirectory::open(&written.0));
    let expected = answers(&Snapshot::open_committed(&local, schema()).unwrap());

    let largest = store
        .files
        .lock()
        .unwrap()
        .values()
        .map(Vec::len)
        .max()
        .unwrap() as u64;
    let cache = Scratch::new("pin");
    let dir = CachedDirectory::new(&cache.0, &store, names.clone(), largest * 2).unwrap();
    let pinned = names[0].clone();
    let remote = EngineDirectory::new(dir);
    let pin = remote.inner().pin(&pinned).unwrap();
    let snapshot = Snapshot::open_committed(&remote, schema()).unwrap();
    assert_eq!(answers(&snapshot), expected);
    // Everything unpinned goes; the pinned file stays on disk.
    remote.inner().evict_all().unwrap();
    assert!(remote.inner().cached_bytes() > 0);
    assert!(cache.0.join(&pinned).exists());
    // A live snapshot is unaffected by the eviction.
    assert_eq!(answers(&snapshot), expected);
    drop(pin);
    remote.inner().evict_all().unwrap();
    assert_eq!(remote.inner().cached_bytes(), 0);
    assert!(!cache.0.join(&pinned).exists());
    assert_eq!(answers(&snapshot), expected);
}

#[test]
fn a_shared_disk_budget_bounds_the_cache_and_overflow_is_served_uncached() {
    let (written, store, names) = uploaded();
    let local = EngineDirectory::new(FsDirectory::open(&written.0));
    let expected = answers(&Snapshot::open_committed(&local, schema()).unwrap());
    let largest = store
        .files
        .lock()
        .unwrap()
        .values()
        .map(Vec::len)
        .max()
        .unwrap() as u64;

    let budget = DiskBudget::new(largest);
    let cache = Scratch::new("budget");
    let dir = CachedDirectory::new(&cache.0, &store, names, 1 << 30)
        .unwrap()
        .with_budget(budget.clone());
    let remote = EngineDirectory::new(dir);
    let snapshot = Snapshot::open_committed(&remote, schema()).unwrap();
    assert_eq!(answers(&snapshot), expected);
    assert!(budget.used() <= largest);
    assert_eq!(budget.used(), remote.inner().cached_bytes());
    remote.inner().evict_all().unwrap();
    assert_eq!(budget.used(), 0, "evicted files return their reservation");

    // A budget smaller than any file: every read is served, none is cached.
    let none = DiskBudget::new(1);
    let cache = Scratch::new("budget0");
    let names2: Vec<String> = store.files.lock().unwrap().keys().cloned().collect();
    let remote = EngineDirectory::new(
        CachedDirectory::new(&cache.0, &store, names2, 1 << 30)
            .unwrap()
            .with_budget(none.clone()),
    );
    let snapshot = Snapshot::open_committed(&remote, schema()).unwrap();
    assert_eq!(answers(&snapshot), expected);
    assert_eq!(remote.inner().cached_bytes(), 0);
    assert!(remote.inner().uncached() > 0);
}

#[test]
fn a_cache_write_that_fails_serves_from_memory_instead_of_failing() {
    let (_written, store, names) = uploaded();
    let cache = Scratch::new("full");
    let dir = CachedDirectory::new(&cache.0, &store, names.clone(), 1 << 30).unwrap();
    // Make the cache directory unwritable by replacing it with a plain file.
    std::fs::remove_dir_all(&cache.0).unwrap();
    std::fs::write(&cache.0, b"not a directory").unwrap();
    let input = dir.open(&names[0]).unwrap();
    assert_eq!(
        input.len(),
        store.files.lock().unwrap()[&names[0]].len(),
        "the bytes still arrive"
    );
    assert_eq!(dir.cached_bytes(), 0);
    assert_eq!(dir.uncached(), 1);
    let _ = std::fs::remove_file(&cache.0);
}

#[test]
fn a_truncated_cache_file_is_refetched_but_a_missing_remote_file_is_an_error() {
    let (_written, store, names) = uploaded();
    let cache = Scratch::new("damage");
    let dir = CachedDirectory::new(&cache.0, &store, names.clone(), 1 << 30).unwrap();
    let name = &names[0];
    let good = dir.open(name).unwrap().len();
    let path = cache.0.join(name);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
    let before = dir.fetches();
    assert_eq!(dir.open(name).unwrap().len(), good);
    assert_eq!(dir.fetches(), before + 1);
    assert_eq!(dir.repaired(), 1);

    // The source vanishing is not a cache miss to paper over.
    store.files.lock().unwrap().remove(name);
    std::fs::remove_file(&path).unwrap();
    assert!(dir.open(name).is_err());
}

#[test]
fn merges_are_admitted_by_slot_and_disk_and_stop_at_shutdown() {
    let scratch = Scratch::new("merge");
    std::fs::create_dir_all(&scratch.0).unwrap();
    let directory = EngineDirectory::new(FsDirectory::open(&scratch.0));
    let engine = Engine::open(&directory, schema(), None).unwrap();
    let fields = [("tag", Value::String("a")), ("n", Value::Integer(1))];
    let put = [Mutation::Put {
        id: "d",
        version: 1,
        fields: &fields,
    }];
    engine.apply(schema().prepare("r", &put).unwrap()).unwrap();
    engine.commit().unwrap();

    // No room for the bytes already on disk: refused before native work.
    let tight = MergeScheduler::new(1, DiskBudget::new(1));
    assert_eq!(engine.merge_scheduled(&tight).unwrap_err(), Error::Limit);
    assert_eq!(tight.running(), 0);

    let budget = DiskBudget::new(1 << 30);
    let scheduler = MergeScheduler::new(1, budget.clone());
    engine.merge_scheduled(&scheduler).unwrap();
    assert_eq!((scheduler.running(), budget.used()), (0, 0));

    scheduler.shutdown();
    assert_eq!(
        engine.merge_scheduled(&scheduler).unwrap_err(),
        Error::Unsupported
    );
    assert!(scheduler.wait_idle(Duration::ZERO));
    engine.close().unwrap();
}
