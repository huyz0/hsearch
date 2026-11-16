//! Remote-read benchmark: GET count, bytes, memory and time for one page-cache
//! configuration, opening and querying a snapshot over an object store.
//! One configuration per process so peak RSS is its own.
//!
//!   cargo run --release -p hsearch-engine --example cache_bench -- <mode> [docs]
//!
//! modes: `profile` (settings + a fixed 20 ms / 100 MB/s store profile), `legacy` (what hsearch used: lucene's defaults, live tuner), `settings`
//! (`RangeCacheSettings::default()`), `nodemand` (settings, demand paging off,
//! spill on), `nodemand-heap` (demand paging off, spill off: whole files read
//! into the heap, the fallback before lucene-rust PR 350).
//! Store: RustFS/S3 when `HSEARCH_S3_ENDPOINT` (+ `_ACCESS_KEY`, `_SECRET_KEY`) is
//! set; otherwise an in-memory backend with `HSEARCH_BENCH_LATENCY_MS` (default 20)
//! of first-byte delay per request. Numbers from either are provisional.
use hsearch_engine::{
    Engine, EngineDirectory, Kind, ManifestEntry, Mutation, ObjectBlobStore, Query,
    RangeCacheSettings, Schema, Snapshot, Sort, Value,
};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Metadata, ObjectBackend, ObjectInfo, ObjectKey, Operation,
    Payload, Precondition, RequestContext, RequestId, VersionToken, WriteReceipt,
};
use lucene_store::{Directory, FsDirectory, PageCacheConfig};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

struct W(std::thread::Thread);
impl Wake for W {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn block_on<T>(f: impl Future<Output = T>) -> T {
    let mut f = std::pin::pin!(f);
    let w = Waker::from(Arc::new(W(std::thread::current())));
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut Context::from_waker(&w)) {
            return v;
        }
        std::thread::park();
    }
}

/// Counts range reads and bytes, and delays each by `latency` (in-memory runs).
struct Counting<B> {
    inner: B,
    gets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    latency: Duration,
    /// Requests in flight now, and how many times one started with none in
    /// flight: the sequential round trips (a lower bound where requests of one
    /// wave are staggered but overlap).
    inflight: Arc<AtomicU64>,
    waves: Arc<AtomicU64>,
}
impl<B: ObjectBackend> ObjectBackend for Counting<B> {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.inner.head(c, k)
    }
    fn create<'a>(
        &'a self,
        c: &'a RequestContext,
        m: &'a Mutation2,
    ) -> Operation<'a, WriteReceipt> {
        self.inner.create(c, m)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(r.length(), Ordering::Relaxed);
        let latency = self.latency;
        if self.inflight.fetch_add(1, Ordering::AcqRel) == 0 {
            self.waves.fetch_add(1, Ordering::Relaxed);
        }
        Box::pin(async move {
            if !latency.is_zero() {
                // The caller blocks on this thread; sleeping here is the wait.
                std::thread::sleep(latency);
            }
            let out = self.inner.read_range(c, k, v, r).await;
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            out
        })
    }
}
type Mutation2 = hsearch_object::Mutation;

fn rss_kib(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn schema() -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword), ("n", Kind::Integer)])
        .unwrap()
        .with_limits(hsearch_engine::Limits::scale())
        .unwrap()
}

fn write_corpus(dir: &std::path::Path, docs: usize) {
    std::fs::create_dir_all(dir).unwrap();
    let directory = EngineDirectory::new(FsDirectory::open(dir));
    let schema = schema();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    let per = 16;
    for batch in 0..docs.div_ceil(per) {
        let lo = batch * per;
        let hi = (lo + per).min(docs);
        let owned: Vec<(String, String, i64)> = (lo..hi)
            .map(|i| {
                let h = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                {
                    // ~600 bytes of incompressible text, so a segment's stored
                    // fields are a multi-MiB file, as real documents make them.
                    let mut x = h;
                    let mut t = format!("t{}-", h >> 44);
                    for _ in 0..40 {
                        x = x
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        t.push_str(&format!("{x:016x}"));
                    }
                    (format!("doc-{i:09}"), t, i as i64)
                }
            })
            .collect();
        let fields: Vec<[(&str, Value<'_>); 2]> = owned
            .iter()
            .map(|(_, t, n)| [("tag", Value::String(t)), ("n", Value::Integer(*n))])
            .collect();
        let mutations: Vec<Mutation<'_>> = owned
            .iter()
            .zip(&fields)
            .map(|((id, _, _), f)| Mutation::Put {
                id,
                version: 1,
                fields: f,
            })
            .collect();
        engine
            .apply(schema.prepare(&format!("b{batch}"), &mutations).unwrap())
            .unwrap();
    }
    engine.commit().unwrap();
    engine.close().unwrap();
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "settings".into());
    let docs: usize = std::env::args()
        .nth(2)
        .and_then(|d| d.parse().ok())
        .unwrap_or(200_000);
    let work = std::env::temp_dir().join(format!("hsearch-cache-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let (src, cache_dir) = (work.join("src"), work.join("cache"));
    write_corpus(&src, docs);
    let files: Vec<(String, Vec<u8>)> = FsDirectory::open(&src)
        .list_all()
        .unwrap()
        .into_iter()
        .filter(|n| n != "write.lock")
        .map(|n| {
            let b = std::fs::read(src.join(&n)).unwrap();
            (n, b)
        })
        .collect();
    let total: usize = files.iter().map(|(_, b)| b.len()).sum();
    let biggest = files.iter().map(|(_, b)| b.len()).max().unwrap_or(0);

    let ctx = RequestContext::new(
        Duration::from_secs(120),
        Cancellation::default(),
        Budget {
            request_bytes: u64::MAX / 4,
            response_bytes: u64::MAX / 4,
            attempts: u32::MAX,
        },
    )
    .unwrap();
    let (gets, bytes) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let waves = Arc::new(AtomicU64::new(0));
    let endpoint = std::env::var("HSEARCH_S3_ENDPOINT").ok();
    let latency_ms: u64 = std::env::var("HSEARCH_BENCH_LATENCY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let (store_name, manifest, backend) = match endpoint {
        Some(endpoint) => {
            let s3 = hsearch_s3::S3Store::new(hsearch_s3::S3Config {
                endpoint,
                bucket: format!("hs-cachebench-{}", std::process::id()),
                access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").unwrap(),
                secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").unwrap(),
                region: "us-east-1".into(),
            })
            .unwrap();
            s3.prepare_bucket(&ctx).unwrap();
            (
                "rustfs",
                upload(&s3, &ctx, &files),
                Backend::S3(Box::new(s3)),
            )
        }
        None => {
            let mem = MemoryStore::new(MemoryLimits {
                objects: 4096,
                versions: 16384,
                version_bytes: 64 << 20,
                identities: 16384,
                identity_bytes: 64 << 20,
            })
            .unwrap();
            ("memory", upload(&mem, &ctx, &files), Backend::Mem(mem))
        }
    };
    let latency = if store_name == "memory" {
        Duration::from_millis(latency_ms)
    } else {
        Duration::ZERO
    };
    let counted = Counting {
        inner: backend,
        gets: gets.clone(),
        bytes: bytes.clone(),
        latency,
        inflight: Arc::default(),
        waves: waves.clone(),
    };
    let store = ObjectBlobStore::new(counted, manifest, Duration::from_secs(60)).unwrap();

    let settings = RangeCacheSettings {
        disk_dir: Some(cache_dir),
        ..RangeCacheSettings::default()
    };
    let config: PageCacheConfig = match mode.as_str() {
        "legacy" => PageCacheConfig {
            disk_dir: settings.disk_dir.clone(),
            ..PageCacheConfig::default()
        },
        "settings" => settings.config().unwrap(),
        "profile" => RangeCacheSettings {
            store_profile: Some(lucene_store::StoreProfile {
                latency: Duration::from_millis(20),
                bytes_per_sec: 100e6,
            }),
            ..settings
        }
        .config()
        .unwrap(),
        "nodemand" => RangeCacheSettings {
            demand_paging: false,
            ..settings
        }
        .config()
        .unwrap(),
        "nodemand-heap" => RangeCacheSettings {
            demand_paging: false,
            spill_min_bytes: None,
            ..settings
        }
        .config()
        .unwrap(),
        other => panic!("unknown mode {other}"),
    };
    let rss0 = rss_kib("VmRSS:");
    let t = Instant::now();
    let cache = Arc::new(lucene_store::PageCache::new(config).unwrap());
    let snapshot = Snapshot::open_blob(store, Arc::clone(&cache), "bench", schema()).unwrap();
    let open = t.elapsed();
    let (open_gets, open_bytes) = (gets.load(Ordering::Relaxed), bytes.load(Ordering::Relaxed));
    let open_waves = waves.load(Ordering::Relaxed);
    let first = {
        let q0 = Instant::now();
        let r = snapshot
            .search(
                &Query::Range {
                    field: "n",
                    lower: Value::Integer(1000),
                    upper: Value::Integer(1500),
                },
                &[Sort::Field {
                    name: "n",
                    descending: false,
                }],
                10,
                &["n"],
            )
            .unwrap();
        let _ = r;
        (
            q0.elapsed(),
            gets.load(Ordering::Relaxed) - open_gets,
            waves.load(Ordering::Relaxed) - open_waves,
        )
    };

    let mut total_hits = 0u64;
    let mut worst = Duration::ZERO;
    let t = Instant::now();
    for i in 0..60u64 {
        let q0 = Instant::now();
        let tag = format!("t{}", (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 44) % 4096);
        let result = if i % 3 == 0 {
            snapshot.search(
                &Query::Range {
                    field: "n",
                    lower: Value::Integer(i as i64 * 1000),
                    upper: Value::Integer(i as i64 * 1000 + 500),
                },
                &[Sort::Field {
                    name: "n",
                    descending: false,
                }],
                10,
                &["n"],
            )
        } else {
            snapshot.search(
                &Query::Term {
                    field: "tag",
                    value: Value::String(&tag),
                },
                &[Sort::Field {
                    name: "n",
                    descending: false,
                }],
                10,
                &["n"],
            )
        }
        .unwrap();
        total_hits += result.total;
        worst = worst.max(q0.elapsed());
    }
    let queries = t.elapsed();
    println!(
        "store={store_name} mode={mode} docs={docs} corpus={:.1}MiB biggest_file={:.1}MiB (provisional)\n  open {:?}: {} GET {:.1} MiB | 60 queries {:?} (worst {:?}): {} GET total {:.1} MiB | hits {} | RSS before {:.0} MiB, after {:.0} MiB, peak {:.0} MiB",
        total as f64 / 1048576.0,
        biggest as f64 / 1048576.0,
        open,
        open_gets,
        open_bytes as f64 / 1048576.0,
        queries,
        worst,
        gets.load(Ordering::Relaxed),
        bytes.load(Ordering::Relaxed) as f64 / 1048576.0,
        total_hits,
        rss0 as f64 / 1024.0,
        rss_kib("VmRSS:") as f64 / 1024.0,
        rss_kib("VmHWM:") as f64 / 1024.0,
    );
    let m = hsearch_engine::RangeCacheMetrics::of(&cache);
    println!(
        "  cold depth: open {} sequential round trips ({} GET); first query {} round trips ({} GET, {:?}) | demand_paging available={} configured={} | heap held {} B in {} whole opens, {} demand-paged opens, {} spill opens",
        open_waves,
        open_gets,
        first.2,
        first.1,
        first.0,
        m.demand_paging_available,
        m.demand_paging_configured,
        m.stats.heap_open_bytes,
        m.stats.heap_opens,
        m.stats.demand_paged_opens,
        m.stats.spill_opens,
    );
    drop(snapshot);
    let _ = std::fs::remove_dir_all(&work);
}

enum Backend {
    S3(Box<hsearch_s3::S3Store>),
    Mem(MemoryStore),
}
impl ObjectBackend for Backend {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        match self {
            Backend::S3(s) => s.head(c, k),
            Backend::Mem(s) => s.head(c, k),
        }
    }
    fn create<'a>(
        &'a self,
        c: &'a RequestContext,
        m: &'a Mutation2,
    ) -> Operation<'a, WriteReceipt> {
        match self {
            Backend::S3(s) => s.create(c, m),
            Backend::Mem(s) => s.create(c, m),
        }
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        match self {
            Backend::S3(s) => s.read_range(c, k, v, r),
            Backend::Mem(s) => s.read_range(c, k, v, r),
        }
    }
}

fn upload(
    store: &impl ObjectBackend,
    ctx: &RequestContext,
    files: &[(String, Vec<u8>)],
) -> Vec<(String, ManifestEntry)> {
    files
        .iter()
        .enumerate()
        .map(|(i, (name, bytes))| {
            let key = ObjectKey::new(name.as_bytes()).unwrap();
            let receipt = block_on(store.create(
                ctx,
                &Mutation2 {
                    request_id: RequestId::new(format!("up-{i}").as_bytes()).unwrap(),
                    key: key.clone(),
                    payload: Payload::new(bytes).unwrap(),
                    metadata: Metadata::new(&[]).unwrap(),
                    precondition: Precondition::Absent,
                },
            ))
            .unwrap();
            (
                name.clone(),
                ManifestEntry {
                    key,
                    version: receipt.object.version,
                    length: bytes.len() as u64,
                },
            )
        })
        .collect()
}
