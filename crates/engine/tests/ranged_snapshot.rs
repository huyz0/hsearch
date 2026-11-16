//! M02.03: committed segments served by range through lucene-rust's own page
//! cache. The only hsearch code between the cache and the object backend is
//! `ObjectBlobStore`, a read-only `BlobStore` over `ObjectBackend` and a manifest;
//! the backend here is a test double that counts, traces and fails range reads.
#[path = "support/remote_fixture.rs"]
mod fixture;

use fixture::{Scratch, answers, write_documents_in};
use hsearch_engine::{
    DEFAULT_SPILL_BUDGET, EngineDirectory, Limits, ManifestEntry, ObjectBlobStore,
    RangeCacheSettings, Schema, Snapshot,
};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error as ObjectError, Metadata, Mutation, ObjectBackend,
    ObjectInfo, ObjectKey, Operation, Payload, Precondition, RequestContext, RequestId,
    VersionToken, WriteReceipt,
};
use lucene_store::{FsDirectory, PageCache, PageCacheConfig};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Counts every call, keeps the range trace, and fails the read with a given index.
#[derive(Default)]
struct Calls {
    heads: Mutex<u64>,
    /// (key, offset, length), in request order.
    ranges: Mutex<Vec<(String, u64, u64)>>,
    fail: Mutex<Option<usize>>,
    /// Which error an injected fault is: 0 unavailable, 1 deadline, 2 throttled.
    fail_kind: Mutex<u8>,
    /// Added to every range read, so concurrent reads overlap in time.
    delay: Mutex<Duration>,
    inflight: std::sync::atomic::AtomicU64,
    /// Requests that started with none in flight: sequential round trips.
    waves: std::sync::atomic::AtomicU64,
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
            let error = match *self.calls.fail_kind.lock().unwrap() {
                1 => ObjectError::DeadlineExceeded,
                2 => ObjectError::Throttled,
                _ => ObjectError::Unavailable,
            };
            return Box::pin(async move { Err(error) });
        }
        let calls = Arc::clone(&self.calls);
        let delay = *calls.delay.lock().unwrap();
        if calls
            .inflight
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            == 0
        {
            calls
                .waves
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let read = self.inner.read_range(c, k, v, r);
        Box::pin(async move {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let out = read.await;
            calls
                .inflight
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            out
        })
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
        // Fixed, not tuned from measured latency: with an in-memory backend the tuner
        // would pick a window of up to `max_request`, which fetches whole files.
        download_window: Some(page),
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

/// Whether lucene-rust can demand-fill 4 KiB pages here (Linux `userfaultfd`).
fn pager_active() -> bool {
    lucene_store::paged_input::Pager::global().is_some_and(|p| p.supports(PAGE))
}

/// No byte is requested twice: per file, the traced ranges never overlap.
fn assert_no_overlap(ranges: &[(String, u64, u64)]) {
    let mut by_file: std::collections::BTreeMap<&str, Vec<(u64, u64)>> = Default::default();
    for (name, offset, len) in ranges {
        by_file
            .entry(name)
            .or_default()
            .push((*offset, offset + len));
    }
    for (name, mut spans) in by_file {
        spans.sort_unstable();
        for pair in spans.windows(2) {
            assert!(
                pair[0].1 <= pair[1].0,
                "{name}: {:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
    }
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
    let budget = f.total / 16;
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
    assert!(
        bytes > budget,
        "the pages read ({bytes} B) exceed the cache ({budget} B), so it had to evict"
    );
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
    assert!(total > 0, "{total}");
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
    let seen = Mutex::new(Vec::new());
    let show = |label: &str| {
        let (n, b) = requests(&f);
        eprintln!(
            "TRACE {label}: {n} range requests, {b} B (manifest {} B in {} files)",
            f.total,
            f.manifest.len()
        );
        seen.lock()
            .unwrap()
            .extend(f.calls.ranges.lock().unwrap().drain(..));
        b
    };
    eprintln!("TRACE pager active: {}", pager_active());
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
    // Everywhere: no byte is requested twice, so the total cannot exceed the manifest.
    assert_no_overlap(&seen.lock().unwrap());
    assert!(fetched <= f.total, "{fetched} of {} B", f.total);
    // Where lucene-rust can demand-fill 4 KiB pages (`userfaultfd`) and read-ahead is
    // fixed at one page, reading is range-native: open plus every query fetch well under
    // half of the files (measured: 27%: 67,671 B at open, 32 KiB for a term query, 4 KiB
    // for a sort). Elsewhere files are read whole, which this test cannot demand.
    if pager_active() {
        assert!(
            fetched * 2 < f.total,
            "range-native: {fetched} of {} B fetched",
            f.total
        );
    }
    assert_eq!(
        cache.stats().fetched_bytes,
        fetched,
        "the cache's counter agrees with the trace"
    );
    assert_eq!(*f.calls.heads.lock().unwrap(), 0);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_store_names_each_files_manifest_version_without_a_request() {
    use lucene_store::BlobStore;
    let f = fixture(10);
    let store = f.store();
    for (name, entry) in &f.manifest {
        assert_eq!(
            store.version(name).unwrap(),
            Some(hex(entry.version.as_bytes())),
            "{name}"
        );
    }
    assert!(store.version("no-such-file").is_err());
    assert_eq!(requests(&f), (0, 0));
    assert_eq!(*f.calls.heads.lock().unwrap(), 0);
}

/// Two published objects with one name and one length but different versions and
/// bytes: a page cached for the first must not answer for the second.
#[test]
fn a_page_cached_under_one_version_is_never_served_for_another() {
    use lucene_store::Directory;
    let publish = |fill: u8, id: &str| {
        let inner = Arc::new(
            MemoryStore::new(MemoryLimits {
                objects: 4,
                versions: 4,
                version_bytes: 1 << 20,
                identities: 4,
                identity_bytes: 1 << 20,
            })
            .unwrap(),
        );
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
        let bytes = vec![fill; 3 * PAGE];
        let key = ObjectKey::new(b"seg").unwrap();
        let receipt = block_on(inner.create(
            &context,
            &Mutation {
                request_id: RequestId::new(id.as_bytes()).unwrap(),
                key: key.clone(),
                payload: Payload::new(&bytes).unwrap(),
                metadata: Metadata::new(&[]).unwrap(),
                precondition: Precondition::Absent,
            },
        ))
        .unwrap();
        let entry = ManifestEntry {
            key,
            version: receipt.object.version,
            length: bytes.len() as u64,
        };
        let backend = Backend {
            inner,
            calls: Arc::default(),
        };
        ObjectBlobStore::new(
            backend,
            [("seg".to_owned(), entry)],
            Duration::from_secs(30),
        )
        .unwrap()
    };
    let dir = Scratch::new("ranged-version");
    let cache = Arc::new(lucene_store::PageCache::new(config(PAGE, u64::MAX, &dir)).unwrap());
    let read = |store| {
        let directory = lucene_store::CachedBlobDirectory::new(store, Arc::clone(&cache), "ns");
        let input = directory.open("seg").unwrap();
        let bytes: &[u8] = &input;
        bytes.to_vec()
    };
    let first = read(publish(0xAA, "a"));
    assert!(first.iter().all(|b| *b == 0xAA));
    let second = read(publish(0xBB, "b"));
    assert!(
        second.iter().all(|b| *b == 0xBB),
        "served the first version's pages"
    );
}

#[test]
fn settings_default_keeps_the_tuner_and_bounds_what_the_fallback_may_spill() {
    let s = RangeCacheSettings::default();
    let c = s.config().unwrap();
    assert!(c.demand_paging);
    assert!(c.store_profile.is_none(), "live tuner stays the default");
    assert_eq!(
        c.spill_min_bytes,
        PageCacheConfig::default().spill_min_bytes
    );
    assert_eq!(c.spill_budget, DEFAULT_SPILL_BUDGET);
    assert!(c.spill_budget < u64::MAX, "spilling must be bounded");
}

#[test]
fn settings_past_the_stores_request_cap_or_with_an_empty_spill_budget_are_refused() {
    let big = RangeCacheSettings {
        max_request: 32 << 20,
        ..RangeCacheSettings::default()
    };
    assert!(matches!(big.config(), Err(hsearch_engine::Error::Limit)));
    let window = RangeCacheSettings {
        download_window: Some(32 << 20),
        ..RangeCacheSettings::default()
    };
    assert!(matches!(window.config(), Err(hsearch_engine::Error::Limit)));
    let none = RangeCacheSettings {
        spill_budget: 0,
        ..RangeCacheSettings::default()
    };
    assert!(matches!(none.config(), Err(hsearch_engine::Error::Limit)));
    let off = RangeCacheSettings {
        spill_min_bytes: None,
        spill_budget: 0,
        ..RangeCacheSettings::default()
    };
    assert!(off.config().is_ok(), "no spilling, no budget needed");
}

/// Both modes, one answer: with demand paging off and spilling on, a snapshot
/// answers like the local one and every byte is still requested at most once.
#[test]
fn the_read_through_fallback_answers_like_local_and_requests_each_byte_once() {
    let f = fixture(1000);
    for demand_paging in [true, false] {
        let dir = Scratch::new("ranged-fallback");
        let calls_before = requests(&f);
        let settings = RangeCacheSettings {
            page_size: PAGE,
            disk_dir: Some(dir.0.clone()),
            demand_paging,
            spill_min_bytes: Some(PAGE as u64),
            spill_budget: f.total * 2,
            download_window: Some(PAGE),
            ..RangeCacheSettings::default()
        };
        let snapshot = Snapshot::open_blob_settings(f.store(), &settings, schema()).unwrap();
        assert_eq!(
            answers(&snapshot),
            f.expected,
            "demand_paging={demand_paging}"
        );
        let (n, bytes) = requests(&f);
        assert!(bytes - calls_before.1 <= f.total, "a byte fetched twice");
        eprintln!(
            "TRACE demand_paging={demand_paging}: {} requests, {} B",
            n - calls_before.0,
            bytes - calls_before.1
        );
    }
}

#[test]
fn a_file_past_the_spill_budget_fails_to_open_and_an_unbounded_disk_tier_is_refused() {
    let f = fixture(1000);
    let dir = Scratch::new("ranged-spill-budget");
    let settings = RangeCacheSettings {
        page_size: PAGE,
        disk_dir: Some(dir.0.clone()),
        demand_paging: false,
        spill_min_bytes: Some(PAGE as u64),
        spill_budget: 1,
        ..RangeCacheSettings::default()
    };
    assert!(Snapshot::open_blob_settings(f.store(), &settings, schema()).is_err());
    let unbounded = RangeCacheSettings {
        disk_dir: Some(dir.0.clone()),
        disk_budget: u64::MAX,
        ..RangeCacheSettings::default()
    };
    assert!(matches!(
        unbounded.config(),
        Err(hsearch_engine::Error::Limit)
    ));
}

#[test]
fn settings_bound_the_heap_and_refuse_an_empty_or_unbounded_budget() {
    let c = RangeCacheSettings::default().config().unwrap();
    assert_eq!(c.heap_budget, hsearch_engine::DEFAULT_HEAP_BUDGET);
    for heap_budget in [0, u64::MAX] {
        let s = RangeCacheSettings {
            heap_budget,
            ..RangeCacheSettings::default()
        };
        assert!(matches!(s.config(), Err(hsearch_engine::Error::Limit)));
    }
}

/// The likely production case (no `userfaultfd`): with demand paging off, the heap
/// held by whole files stays within `heap_budget`, a budget below what a snapshot
/// must hold whole fails the open rather than growing the heap, and the metrics say
/// which path runs.
#[test]
fn without_demand_paging_heap_residency_stays_within_its_budget() {
    let f = fixture(1000);
    let dir = Scratch::new("ranged-heap-budget");
    let settings = RangeCacheSettings {
        page_size: PAGE,
        disk_dir: Some(dir.0.clone()),
        demand_paging: false,
        spill_min_bytes: Some(PAGE as u64),
        spill_budget: f.total * 2,
        heap_budget: f.total,
        download_window: Some(PAGE),
        ..RangeCacheSettings::default()
    };
    let cache = Arc::new(PageCache::new(settings.config().unwrap()).unwrap());
    let snapshot =
        Snapshot::open_blob(f.store(), Arc::clone(&cache), "heap-budget", schema()).unwrap();
    assert_eq!(answers(&snapshot), f.expected);
    let m = hsearch_engine::RangeCacheMetrics::of(&cache);
    assert!(
        !m.demand_paging_active(),
        "demand paging was configured off"
    );
    assert!(m.stats.heap_open_bytes <= settings.heap_budget);
    assert!(
        m.stats.heap_opens > 0,
        "pinned and small files are held whole"
    );
    assert_eq!(m.stats.demand_paged_opens, 0);
    eprintln!(
        "TRACE heap: {} B of {} B corpus held, {} whole opens, {} spills",
        m.stats.heap_open_bytes, f.total, m.stats.heap_opens, m.stats.spill_opens
    );
    drop(snapshot);

    let tight = RangeCacheSettings {
        heap_budget: 1,
        ..settings
    };
    let dir = Scratch::new("ranged-heap-tight");
    let tight = RangeCacheSettings {
        disk_dir: Some(dir.0.clone()),
        ..tight
    };
    assert!(
        Snapshot::open_blob_settings(f.store(), &tight, schema()).is_err(),
        "a budget below the whole-file set refuses the open"
    );
}

#[test]
fn metrics_report_demand_paging_as_available_only_where_the_pager_runs() {
    let cache = PageCache::new(RangeCacheSettings::default().config().unwrap()).unwrap();
    let m = hsearch_engine::RangeCacheMetrics::of(&cache);
    assert_eq!(
        m.demand_paging_available,
        lucene_store::paged_input::Pager::global().is_some()
    );
    assert!(m.demand_paging_configured);
}

/// Faults of every kind a remote read can see (unavailable, deadline, throttled),
/// injected at each request while the disk tier is a quarter of the corpus: the
/// outcome is an error or the exact answer, never a wrong one, and a fresh open
/// after the fault recovers.
#[test]
fn timeouts_and_throttling_under_a_cache_smaller_than_the_corpus_never_give_a_wrong_answer() {
    let f = fixture(1000);
    let budget = f.total / 4;
    let total = {
        let dir = Scratch::new("ranged-small-count");
        let s = Snapshot::open_blob_with(f.store(), config(PAGE, budget, &dir), schema()).unwrap();
        assert_eq!(answers(&s), f.expected);
        requests(&f).0
    };
    for kind in [1u8, 2] {
        *f.calls.fail_kind.lock().unwrap() = kind;
        for fail in 0..total {
            f.calls.ranges.lock().unwrap().clear();
            *f.calls.fail.lock().unwrap() = Some(fail);
            let dir = Scratch::new("ranged-small-fail");
            let outcome = Snapshot::open_blob_with(f.store(), config(PAGE, budget, &dir), schema())
                .map(|s| answers(&s));
            if let Ok(got) = outcome {
                assert_eq!(got, f.expected, "kind {kind} fail at {fail}: wrong answer");
            }
            *f.calls.fail.lock().unwrap() = None;
            let dir = Scratch::new("ranged-small-retry");
            let s =
                Snapshot::open_blob_with(f.store(), config(PAGE, budget, &dir), schema()).unwrap();
            assert_eq!(answers(&s), f.expected, "retry after kind {kind} at {fail}");
        }
    }
}

/// The cold path in sequential round trips, which PR-04 and PR-31 depend on
/// (count alone hides depth): with each read taking time, opening a snapshot of
/// 33 files and running its first query overlap their reads into few waves.
/// Provisional bounds from in-memory runs; the RustFS numbers are in the M04.02
/// evidence.
#[test]
fn a_cold_open_and_first_query_take_few_sequential_round_trips() {
    use std::sync::atomic::Ordering;
    let f = fixture(1000);
    *f.calls.delay.lock().unwrap() = Duration::from_millis(3);
    let dir = Scratch::new("ranged-depth");
    let settings = RangeCacheSettings {
        disk_dir: Some(dir.0.clone()),
        store_profile: Some(lucene_store::StoreProfile {
            latency: Duration::from_millis(20),
            bytes_per_sec: 100e6,
        }),
        ..RangeCacheSettings::default()
    };
    let snapshot = Snapshot::open_blob_settings(f.store(), &settings, schema()).unwrap();
    let open = f.calls.waves.load(Ordering::Relaxed);
    let first = answers(&snapshot);
    let total = f.calls.waves.load(Ordering::Relaxed);
    eprintln!(
        "TRACE depth: open {open} round trips, answers {} more, {} requests",
        total - open,
        requests(&f).0
    );
    assert_eq!(first, f.expected);
    assert!(open <= 4, "open took {open} sequential round trips");
    assert!(
        total - open <= 3,
        "the all-query set took {} more",
        total - open
    );
}
