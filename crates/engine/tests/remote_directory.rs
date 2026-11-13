//! Search over remote-backed segments equals search over a local directory, and
//! losing or evicting the cache never changes it.
#[path = "support/remote_fixture.rs"]
mod fixture;

use fixture::{Scratch, answers, schema, write_documents};
use hsearch_engine::{CachedDirectory, EngineDirectory, Query, RemoteError, RemoteFiles, Snapshot};
use lucene_store::{Directory, FsDirectory};
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
struct Store {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    down: Mutex<bool>,
}
impl RemoteFiles for &Store {
    fn fetch(&self, name: &str) -> Result<Vec<u8>, RemoteError> {
        if *self.down.lock().unwrap() {
            return Err(RemoteError::Unavailable);
        }
        self.files
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or(RemoteError::NotFound)
    }
}

fn write_and_upload(store: &Store) -> (Scratch, Vec<String>) {
    let (scratch, files) = write_documents();
    let mut names = Vec::new();
    for (name, bytes) in files {
        store.files.lock().unwrap().insert(name.clone(), bytes);
        names.push(name);
    }
    (scratch, names)
}

#[test]
fn remote_segments_answer_like_a_local_directory_and_survive_cache_loss() {
    let store = Store::default();
    let (written, names) = write_and_upload(&store);

    let local = EngineDirectory::new(FsDirectory::open(&written.0));
    let expected = answers(&Snapshot::open_committed(&local, schema()).unwrap());
    assert_eq!(expected[0].0, 40);
    assert!(expected[1].0 > 0 && expected[2].0 == 13);

    let cache = Scratch::new("cache");
    let remote = EngineDirectory::new(
        CachedDirectory::new(&cache.0, &store, names.clone(), 64 * 1024 * 1024).unwrap(),
    );
    assert_eq!(
        answers(&Snapshot::open_committed(&remote, schema()).unwrap()),
        expected
    );
    let first = remote.inner().fetches();
    assert!(
        first > 0 && first <= names.len() as u64,
        "at most one fetch per file"
    );

    // A second open is served from the cache: no more fetches.
    assert_eq!(
        answers(&Snapshot::open_committed(&remote, schema()).unwrap()),
        expected
    );
    assert_eq!(remote.inner().fetches(), first);

    // Deleting every cached file changes nothing but the fetch count.
    remote.inner().evict_all().unwrap();
    assert_eq!(remote.inner().cached_bytes(), 0);
    assert_eq!(
        answers(&Snapshot::open_committed(&remote, schema()).unwrap()),
        expected
    );
    assert_eq!(remote.inner().fetches(), 2 * first);

    // The same, by deleting the files behind the directory's back.
    for entry in std::fs::read_dir(&cache.0).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    remote.inner().evict_all().unwrap();
    assert_eq!(
        answers(&Snapshot::open_committed(&remote, schema()).unwrap()),
        expected
    );
}

#[test]
fn a_small_cache_evicts_and_still_answers() {
    let store = Store::default();
    let (_written, names) = write_and_upload(&store);
    let largest = store
        .files
        .lock()
        .unwrap()
        .values()
        .map(Vec::len)
        .max()
        .unwrap() as u64;
    let cache = Scratch::new("small");
    let directory = CachedDirectory::new(&cache.0, &store, names, largest).unwrap();
    let remote = EngineDirectory::new(directory);
    let snapshot = Snapshot::open_committed(&remote, schema()).unwrap();
    assert_eq!(
        snapshot.search(&Query::All, &[], 100, &[]).unwrap().total,
        40
    );
    assert!(remote.inner().cached_bytes() <= largest);
}

#[test]
fn file_larger_than_the_cache_bound_and_store_failures_are_errors() {
    let store = Store::default();
    let (_written, names) = write_and_upload(&store);
    let cache = Scratch::new("tiny");
    let tiny =
        EngineDirectory::new(CachedDirectory::new(&cache.0, &store, names.clone(), 1).unwrap());
    assert!(Snapshot::open_committed(&tiny, schema()).is_err());

    let cache = Scratch::new("down");
    let down = EngineDirectory::new(
        CachedDirectory::new(&cache.0, &store, names.clone(), 1 << 26).unwrap(),
    );
    *store.down.lock().unwrap() = true;
    assert!(Snapshot::open_committed(&down, schema()).is_err());
    *store.down.lock().unwrap() = false;
    assert!(
        Snapshot::open_committed(&down, schema()).is_ok(),
        "recovers once the store does"
    );

    let cache = Scratch::new("missing");
    let mut with_ghost = names;
    with_ghost.push("ghost.bin".into());
    let ghost = CachedDirectory::new(&cache.0, &store, with_ghost, 1 << 26).unwrap();
    assert!(ghost.open("ghost.bin").is_err());
    assert!(ghost.list_all().unwrap().contains(&"ghost.bin".to_string()));
}

#[test]
fn the_cache_directory_is_emptied_on_creation_and_deletes_forget_remote_files() {
    let store = Store::default();
    let (_written, names) = write_and_upload(&store);
    let cache = Scratch::new("stale");
    std::fs::create_dir_all(&cache.0).unwrap();
    std::fs::write(cache.0.join("segments_1"), b"stale and wrong").unwrap();
    std::fs::create_dir_all(cache.0.join("sub")).unwrap();
    let directory = CachedDirectory::new(&cache.0, &store, names.clone(), 1 << 26).unwrap();
    assert!(
        std::fs::read_dir(directory.cache_root())
            .unwrap()
            .next()
            .is_none()
    );
    let victim = names[0].clone();
    directory.open(&victim).unwrap();
    directory.delete_file(&victim).unwrap();
    assert!(!directory.list_all().unwrap().contains(&victim));
    assert_eq!(directory.cached_bytes(), 0);
    // The store's copy is untouched: marking it remote again makes it readable.
    directory.mark_remote(&victim).unwrap();
    assert!(directory.open(&victim).is_ok());
    // Local-only files are plain files.
    let mut out = directory.create_output("local-only").unwrap();
    lucene_store::DataOutput::write_bytes(&mut out, b"x");
    out.close().unwrap();
    assert!(
        directory
            .list_all()
            .unwrap()
            .contains(&"local-only".to_string())
    );
    directory.delete_file("local-only").unwrap();
}
