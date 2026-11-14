//! M02.03: committed segments served by range through lucene-rust's own page
//! cache. The only hsearch code between the cache and the object backend is
//! `ObjectBlobStore`, a read-only `BlobStore` over `ObjectBackend` and a manifest;
//! the backend here is a test double that counts, traces and fails range reads.
#[path = "support/remote_fixture.rs"]
mod fixture;

use fixture::{Scratch, answers, write_documents_in};
use hsearch_engine::{EngineDirectory, Limits, ManifestEntry, ObjectBlobStore, Schema, Snapshot};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error as ObjectError, Metadata, Mutation, ObjectBackend,
    ObjectInfo, ObjectKey, Operation, Payload, Precondition, RequestContext, RequestId,
    VersionToken, WriteReceipt,
};
use lucene_store::{FsDirectory, PageCacheConfig};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Counts every call, keeps the range trace, and fails the read with a given index.
#[derive(Default)]
struct Calls {
    heads: Mutex<u64>,
    /// (key, offset, length), in request order.
    ranges: Mutex<Vec<(String, u64, u64)>>,
    fail: Mutex<Option<usize>>,
}

struct Backend {
    inner: Arc<MemoryStore>,
    calls: Arc<Calls>,
}

impl ObjectBackend for Backend {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        *self.calls.heads.lock().unwrap() += 1;
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, m: &'a Mutation) -> Operation<'a, WriteReceipt> {
        self.inner.create(c, m)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        let index = {
            let mut ranges = self.calls.ranges.lock().unwrap();
            ranges.push((
                String::from_utf8_lossy(k.as_bytes()).into_owned(),
                r.offset(),
                r.length(),
            ));
            ranges.len() - 1
        };
        if *self.calls.fail.lock().unwrap() == Some(index) {
            return Box::pin(async { Err(ObjectError::Unavailable) });
        }
        self.inner.read_range(c, k, v, r)
    }
}

fn schema() -> Schema {
    fixture::schema().with_limits(Limits::scale()).unwrap()
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    use std::task::{Context, Poll, Wake, Waker};
    struct W(std::thread::Thread);
    impl Wake for W {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(W(std::thread::current())));
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut Context::from_waker(&waker)) {
            return v;
        }
        std::thread::park();
    }
}

struct Fixture {
    calls: Arc<Calls>,
    inner: Arc<MemoryStore>,
    manifest: Vec<(String, ManifestEntry)>,
    expected: Vec<(u64, Vec<String>)>,
    total: u64,
    _scratch: Scratch,
}

impl Fixture {
    /// A store sharing this fixture's counters, over the same objects.
    fn store(&self) -> ObjectBlobStore<Backend> {
        let backend = Backend {
            inner: Arc::clone(&self.inner),
            calls: Arc::clone(&self.calls),
        };
        ObjectBlobStore::new(backend, self.manifest.clone(), Duration::from_secs(30)).unwrap()
    }
}

fn fixture(batches: usize) -> Fixture {
    let started = Instant::now();
    let (scratch, files) = write_documents_in(batches);
    eprintln!(
        "TRACE fixture: {} files written in {:?}",
        files.len(),
        started.elapsed()
    );
    let total = files.iter().map(|(_, b)| b.len() as u64).sum();
    let local = EngineDirectory::new(FsDirectory::open(&scratch.0));
    let expected = answers(&Snapshot::open_committed(&local, schema()).unwrap());
    let limits = MemoryLimits {
        objects: 4096,
        versions: 16384,
        version_bytes: 64 * 1024 * 1024,
        identities: 16384,
        identity_bytes: 64 * 1024 * 1024,
    };
    let inner = Arc::new(MemoryStore::new(limits).unwrap());
    let context = RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: u64::MAX / 4,
            response_bytes: u64::MAX / 4,
            attempts: u32::MAX,
        },
    )
    .unwrap();
    let mut manifest = Vec::new();
    for (i, (name, bytes)) in files.iter().enumerate() {
        let key = ObjectKey::new(name.as_bytes()).unwrap();
        let receipt = block_on(inner.create(
            &context,
            &Mutation {
                request_id: RequestId::new(format!("publish-{i}").as_bytes()).unwrap(),
                key: key.clone(),
                payload: Payload::new(bytes).unwrap(),
                metadata: Metadata::new(&[]).unwrap(),
                precondition: Precondition::Absent,
            },
        ))
        .unwrap();
        manifest.push((
            name.clone(),
            ManifestEntry {
                key,
                version: receipt.object.version,
                length: bytes.len() as u64,
            },
        ));
    }
    Fixture {
        calls: Arc::default(),
        inner,
        manifest,
        expected,
        total,
        _scratch: scratch,
    }
}

/// lucene-rust pages by demand only when the page is a multiple of the OS page
/// (4 KiB); any other size makes it read each file whole at open.
const PAGE: usize = 4096;

fn config(page: usize, budget: u64, dir: &Scratch) -> PageCacheConfig {
    PageCacheConfig {
        page_size: page,
        disk_dir: Some(dir.0.clone()),
        disk_budget: budget,
        pinned_extensions: Vec::new(),
        ..PageCacheConfig::default()
    }
}

#[test]
fn ranged_snapshot_answers_like_local_with_no_list_and_no_head() {
    let f = fixture(100);
    let cache = Scratch::new("ranged-a");
    let started = Instant::now();
    let snapshot =
        Snapshot::open_blob_with(f.store(), config(1024, u64::MAX, &cache), schema()).unwrap();
    eprintln!(
        "TRACE open {:?}, ranges {}",
        started.elapsed(),
        f.calls.ranges.lock().unwrap().len()
    );
    assert_eq!(answers(&snapshot), f.expected);
    eprintln!(
        "TRACE queried {:?}, ranges {}",
        started.elapsed(),
        f.calls.ranges.lock().unwrap().len()
    );
    assert_eq!(*f.calls.heads.lock().unwrap(), 0, "no HEAD");
}

fn requests(f: &Fixture) -> (usize, u64) {
    let ranges = f.calls.ranges.lock().unwrap();
    (ranges.len(), ranges.iter().map(|r| r.2).sum())
}

#[test]
fn a_shared_cache_serves_a_second_open_without_any_request() {
    let f = fixture(1000);
    let dir = Scratch::new("ranged-share");
    let cache = Arc::new(lucene_store::PageCache::new(config(PAGE, u64::MAX, &dir)).unwrap());
    let first = Snapshot::open_blob(f.store(), Arc::clone(&cache), "ns", schema()).unwrap();
    assert_eq!(answers(&first), f.expected);
    let (n, bytes) = requests(&f);
    eprintln!(
        "TRACE first open: {n} range requests, {bytes} B of {} B in {} files",
        f.total,
        f.manifest.len()
    );
    assert!(
        n > 0 && bytes <= f.total,
        "each page is fetched at most once"
    );
    let second = Snapshot::open_blob(f.store(), Arc::clone(&cache), "ns", schema()).unwrap();
    assert_eq!(answers(&second), f.expected);
    assert_eq!(
        requests(&f),
        (n, bytes),
        "the second open is served from the cache"
    );
    assert_eq!(
        *f.calls.heads.lock().unwrap(),
        0,
        "no HEAD: lengths come from the manifest"
    );
    let stats = cache.stats();
    assert_eq!(stats.requests as usize, n);
    assert!(stats.disk_hits > 0);
}

#[test]
fn working_set_larger_than_the_cache_stays_bounded_and_correct() {
    let f = fixture(1000);
    let budget = f.total / 8;
    assert!(budget >= 4096, "fixture too small to page: {}", f.total);
    let dir = Scratch::new("ranged-bound");
    let cache = Arc::new(lucene_store::PageCache::new(config(PAGE, budget, &dir)).unwrap());
    let snapshot = Snapshot::open_blob(f.store(), Arc::clone(&cache), "ns", schema()).unwrap();
    for _ in 0..3 {
        assert_eq!(answers(&snapshot), f.expected);
        assert!(
            cache.stats().disk_footprint_bytes <= budget,
            "{:?}",
            cache.stats()
        );
    }
    let (n, bytes) = requests(&f);
    eprintln!(
        "TRACE bounded: working set {} B, disk budget {budget} B, {n} range requests, {bytes} B, footprint {} B",
        f.total,
        cache.stats().disk_footprint_bytes
    );
}

#[test]
fn an_injected_range_error_is_an_error_never_a_wrong_answer_and_a_retry_recovers() {
    let f = fixture(1000);
    let total = {
        let dir = Scratch::new("ranged-count");
        let s =
            Snapshot::open_blob_with(f.store(), config(PAGE, u64::MAX, &dir), schema()).unwrap();
        assert_eq!(answers(&s), f.expected);
        requests(&f).0
    };
    assert!(total >= 10, "{total}");
    for fail in 0..total {
        f.calls.ranges.lock().unwrap().clear();
        *f.calls.fail.lock().unwrap() = Some(fail);
        let dir = Scratch::new("ranged-fail");
        let outcome = Snapshot::open_blob_with(f.store(), config(PAGE, u64::MAX, &dir), schema())
            .map(|s| answers(&s));
        match outcome {
            Ok(got) => assert_eq!(got, f.expected, "fail at {fail} gave a wrong answer"),
            Err(_) => {
                // A fresh attempt with the fault spent succeeds and is exact.
                *f.calls.fail.lock().unwrap() = None;
                let dir = Scratch::new("ranged-retry");
                let s = Snapshot::open_blob_with(f.store(), config(PAGE, u64::MAX, &dir), schema())
                    .unwrap();
                assert_eq!(answers(&s), f.expected, "retry after fail at {fail}");
            }
        }
    }
    eprintln!("TRACE injected an error at each of {total} range requests: never a wrong answer");
}

#[test]
fn a_wrong_version_or_a_missing_file_is_an_error_and_there_is_no_list() {
    let f = fixture(20);
    let dir = Scratch::new("ranged-ver");
    let mut stale = f.manifest.clone();
    stale[0].1.version = VersionToken::new(b"no-such-version").unwrap();
    let backend = Backend {
        inner: Arc::clone(&f.inner),
        calls: Arc::clone(&f.calls),
    };
    let store = ObjectBlobStore::new(backend, stale, Duration::from_secs(30)).unwrap();
    assert!(Snapshot::open_blob_with(store, config(PAGE, u64::MAX, &dir), schema()).is_err());
    let backend = Backend {
        inner: Arc::clone(&f.inner),
        calls: Arc::clone(&f.calls),
    };
    let none = ObjectBlobStore::new(backend, Vec::new(), Duration::from_secs(30)).unwrap();
    assert!(Snapshot::open_blob_with(none, config(PAGE, u64::MAX, &dir), schema()).is_err());
    let backend = Backend {
        inner: Arc::clone(&f.inner),
        calls: Arc::clone(&f.calls),
    };
    let long = Duration::from_secs(301);
    assert!(ObjectBlobStore::new(backend, f.manifest.clone(), long).is_err());
}

#[test]
fn per_query_file_access_trace_is_consistent_and_fetches_no_page_twice() {
    use hsearch_engine::{Query, Sort, Value};
    let f = fixture(2000);
    let dir = Scratch::new("ranged-trace");
    let cache = Arc::new(lucene_store::PageCache::new(config(PAGE, u64::MAX, &dir)).unwrap());
    let snapshot = Snapshot::open_blob(f.store(), Arc::clone(&cache), "ns", schema()).unwrap();
    let show = |label: &str| {
        let (n, b) = requests(&f);
        eprintln!(
            "TRACE {label}: {n} range requests, {b} B (manifest {} B in {} files)",
            f.total,
            f.manifest.len()
        );
        f.calls.ranges.lock().unwrap().clear();
        b
    };
    let mut fetched = show("open");
    let term = Query::Term {
        field: "tag",
        value: Value::String("t1"),
    };
    let range = Query::Range {
        field: "n",
        lower: Value::Integer(5),
        upper: Value::Integer(17),
    };
    let by_n = [Sort::Field {
        name: "n",
        descending: false,
    }];
    snapshot.search(&term, &[], 10, &[]).unwrap();
    fetched += show("term query");
    snapshot.search(&range, &[], 10, &[]).unwrap();
    fetched += show("range query");
    snapshot.search(&Query::All, &by_n, 10, &[]).unwrap();
    fetched += show("all + doc-values sort");
    snapshot.search(&Query::All, &[], 10, &["n"]).unwrap();
    fetched += show("all + stored-field projection");
    snapshot.search(&term, &[], 10, &[]).unwrap();
    assert_eq!(
        show("term query again (warm)"),
        0,
        "a warm query sends nothing"
    );
    // Laziness is measured and printed, not asserted: it needs `userfaultfd` and the
    // process-wide pager, so it varies between environments and between runs. What
    // must hold everywhere: no page is fetched twice.
    assert!(fetched <= f.total, "{fetched} of {} B", f.total);
    assert_eq!(
        cache.stats().fetched_bytes,
        fetched,
        "the cache's counter agrees with the trace"
    );
    assert_eq!(*f.calls.heads.lock().unwrap(), 0);
}
