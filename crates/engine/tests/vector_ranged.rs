//! M23.03: vector indexes served by range from an object store through the
//! page cache. A corpus larger than the cache is written with the engine,
//! published to a counting backend, and queried cold and warm; the report is
//! range requests, bytes and sequential round trips per query, and recall
//! against an exact scan of the same corpus.
//!
//! Provisional: an in-memory backend that counts and delays, on one shared VM.
//! The counts do not depend on the machine; the times do.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hsearch_engine::{
    CodeQuantizer, Engine, EngineDirectory, Kind, Limits, ManifestEntry, Mutation, ObjectBlobStore,
    Query, RangeCacheSettings, Rerank, Schema, Similarity, Snapshot, Sort, Value, VectorSpec,
};
use hsearch_object::{
    ByteRange, Error as ObjectError, Mutation as ObjectMutation, ObjectBackend, ObjectInfo,
    ObjectKey, Operation, Payload, RequestContext, VersionToken, WriteReceipt,
};
use lucene_store::{Directory, FsDirectory};

const DIM: usize = 128;
const PAGE: usize = 4096;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn normal(&mut self) -> f32 {
        (0..12)
            .map(|_| (self.next() >> 40) as f32 / (1u64 << 24) as f32)
            .sum::<f32>()
            - 6.0
    }
}

fn corpus(n: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    let centres: Vec<Vec<f32>> = (0..64)
        .map(|_| (0..DIM).map(|_| rng.normal() * 3.0).collect())
        .collect();
    (0..n)
        .map(|i| centres[i % 64].iter().map(|c| c + rng.normal()).collect())
        .collect()
}

/// Counts every range request, with its size and the time since the previous
/// one had finished (a request that starts with none in flight is a new
/// sequential round trip).
#[derive(Default)]
struct Calls {
    ranges: Mutex<Vec<(String, u64, u64)>>,
    inflight: AtomicU64,
    waves: AtomicU64,
    delay: Mutex<Duration>,
}

/// Serves the committed files of a local directory as objects, so an index
/// larger than the in-memory store's cap can be read by range.
struct Backend {
    root: std::path::PathBuf,
    calls: Arc<Calls>,
}

impl ObjectBackend for Backend {
    fn head<'a>(&'a self, _: &'a RequestContext, _: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        Box::pin(async { Err(ObjectError::Unavailable) })
    }
    fn create<'a>(
        &'a self,
        _: &'a RequestContext,
        _: &'a ObjectMutation,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async { Err(ObjectError::Unavailable) })
    }
    fn read_range<'a>(
        &'a self,
        _: &'a RequestContext,
        k: &'a ObjectKey,
        _: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        let name = String::from_utf8_lossy(k.as_bytes()).into_owned();
        self.calls
            .ranges
            .lock()
            .unwrap()
            .push((name.clone(), r.offset(), r.length()));
        let calls = Arc::clone(&self.calls);
        if calls.inflight.fetch_add(1, Ordering::AcqRel) == 0 {
            calls.waves.fetch_add(1, Ordering::Relaxed);
        }
        let delay = *calls.delay.lock().unwrap();
        let path = self.root.join(&name);
        Box::pin(async move {
            use std::os::unix::fs::FileExt;
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let mut bytes = vec![0u8; r.length() as usize];
            let read = std::fs::File::open(&path)
                .and_then(|f| f.read_exact_at(&mut bytes, r.offset()))
                .map_err(|_| ObjectError::Unavailable);
            calls.inflight.fetch_sub(1, Ordering::AcqRel);
            read?;
            Payload::new(&bytes)
        })
    }
}

struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "hsearch-vranged-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn schema(spec: VectorSpec) -> Schema {
    Schema::new(1, &[("embedding", Kind::Vector(spec))])
        .unwrap()
        .with_limits(Limits::scale().with_document_bytes(DIM * 4 + 1024))
        .unwrap()
}

/// A committed index of `vectors`, published to a counting backend.
struct Published {
    calls: Arc<Calls>,
    manifest: Vec<(String, ManifestEntry)>,
    total: u64,
    _scratch: Scratch,
}

impl Published {
    fn store(&self) -> ObjectBlobStore<Backend> {
        let backend = Backend {
            root: self._scratch.0.clone(),
            calls: Arc::clone(&self.calls),
        };
        ObjectBlobStore::new(backend, self.manifest.clone(), Duration::from_secs(30)).unwrap()
    }

    /// Files this index holds, by extension, with their sizes.
    fn sizes(&self) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for (name, entry) in &self.manifest {
            let extension = name.rsplit('.').next().unwrap_or("").to_string();
            *out.entry(extension).or_insert(0) += entry.length;
        }
        out
    }
}

fn publish(spec: VectorSpec, vectors: &[Vec<f32>]) -> Published {
    let scratch = Scratch::new("write");
    let directory = EngineDirectory::new(FsDirectory::open(&scratch.0));
    let schema = schema(spec);
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    for (b, chunk) in vectors.chunks(16).enumerate() {
        let ids: Vec<String> = (0..chunk.len())
            .map(|j| format!("d{}", b * 16 + j))
            .collect();
        let fields: Vec<[(&str, Value); 1]> = chunk
            .iter()
            .map(|v| [("embedding", Value::Vector(v))])
            .collect();
        let mutations: Vec<Mutation> = ids
            .iter()
            .zip(&fields)
            .map(|(id, f)| Mutation::Put {
                id,
                version: 1,
                fields: f,
            })
            .collect();
        engine
            .apply(schema.prepare(&format!("b{b}"), &mutations).unwrap())
            .unwrap();
    }
    // One segment: a query reads one index, as a merged segment would.
    engine.refresh().unwrap();
    engine.merge().unwrap();
    engine.commit().unwrap();
    engine.close().unwrap();

    let local = FsDirectory::open(&scratch.0);
    let mut manifest = Vec::new();
    let mut total = 0;
    for name in local.list_all().unwrap() {
        if name == "write.lock" {
            continue;
        }
        let length = std::fs::metadata(scratch.0.join(&name)).unwrap().len();
        total += length;
        manifest.push((
            name.clone(),
            ManifestEntry {
                key: ObjectKey::new(name.as_bytes()).unwrap(),
                version: VersionToken::new(b"v1").unwrap(),
                length,
            },
        ));
    }
    Published {
        calls: Arc::default(),
        manifest,
        total,
        _scratch: scratch,
    }
}

fn exact(vectors: &[Vec<f32>], query: &[f32], k: usize) -> Vec<String> {
    let mut scored: Vec<(f32, usize)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| (v.iter().zip(query).map(|(a, b)| (a - b) * (a - b)).sum(), i))
        .collect();
    scored.sort_by(|a, b| a.partial_cmp(b).unwrap());
    scored
        .iter()
        .take(k)
        .map(|(_, i)| format!("d{i}"))
        .collect()
}

/// What a pass over the queries cost the backend.
#[derive(Clone, Copy, Debug, Default)]
struct Pass {
    /// Range requests, all queries.
    requests: u64,
    bytes: u64,
    /// Sequential round trips (requests started with none in flight).
    waves: u64,
    /// Most requests one query made.
    worst_requests: u64,
    worst_waves: u64,
    recall: f64,
}

fn run_pass(
    published: &Published,
    snapshot: &Snapshot,
    vectors: &[Vec<f32>],
    queries: &[Vec<f32>],
    (beam, nprobe): (usize, usize),
) -> Pass {
    let mut pass = Pass::default();
    let mut found = 0;
    for q in queries {
        let (requests_before, bytes_before) = {
            let r = published.calls.ranges.lock().unwrap();
            (r.len() as u64, r.iter().map(|x| x.2).sum::<u64>())
        };
        let waves_before = published.calls.waves.load(Ordering::Relaxed);
        let hits = snapshot
            .search(
                &Query::Knn {
                    field: "embedding",
                    vector: q,
                    k: 10,
                    candidates: beam,
                    nprobe,
                    filter: None,
                },
                &[Sort::Score],
                10,
                &[],
            )
            .unwrap()
            .hits;
        let (requests_after, bytes_after) = {
            let r = published.calls.ranges.lock().unwrap();
            (r.len() as u64, r.iter().map(|x| x.2).sum::<u64>())
        };
        let waves = published.calls.waves.load(Ordering::Relaxed) - waves_before;
        let requests = requests_after - requests_before;
        pass.requests += requests;
        pass.bytes += bytes_after - bytes_before;
        pass.waves += waves;
        pass.worst_requests = pass.worst_requests.max(requests);
        pass.worst_waves = pass.worst_waves.max(waves);
        let want = exact(vectors, q, 10);
        found += want
            .iter()
            .filter(|id| hits.iter().any(|h| &h.id == *id))
            .count();
    }
    pass.recall = found as f64 / (10 * queries.len()) as f64;
    pass
}

/// One variant: open over a cache a fraction of the index, then report
/// the open, a cold pass and a warm pass.
struct Report {
    name: &'static str,
    total: u64,
    open_requests: u64,
    open_bytes: u64,
    cold: Pass,
    warm: Pass,
    steady: Pass,
}

fn measure(
    name: &'static str,
    warm_on_open: bool,
    spec: VectorSpec,
    (beam, nprobe): (usize, usize),
    vectors: &[Vec<f32>],
    (queries, fresh): (&[Vec<f32>], &[Vec<f32>]),
    budget: u64,
) -> Report {
    let published = publish(spec, vectors);
    let dir = Scratch::new("cache");
    let settings = RangeCacheSettings {
        page_size: PAGE,
        disk_dir: Some(dir.0.clone()),
        disk_budget: budget,
        download_window: Some(PAGE),
        warm_on_open,
        ..RangeCacheSettings::default()
    };
    *published.calls.delay.lock().unwrap() = Duration::from_millis(1);
    let snapshot =
        Snapshot::open_blob_settings(published.store(), &settings, schema(spec)).unwrap();
    let (open_requests, open_bytes) = {
        let r = published.calls.ranges.lock().unwrap();
        (r.len() as u64, r.iter().map(|x| x.2).sum::<u64>())
    };
    let cold = run_pass(&published, &snapshot, vectors, queries, (beam, nprobe));
    let warm = run_pass(&published, &snapshot, vectors, queries, (beam, nprobe));
    // Other queries over the warmed cache: what a steady workload pays.
    let steady = run_pass(&published, &snapshot, vectors, fresh, (beam, nprobe));
    eprintln!(
        "{name}: index {:.1} MiB {:?}; cache {:.1} MiB; open {open_requests} requests {:.1} MiB; \
         cold per query {:.1} requests {:.1} KiB {:.1} round trips (worst {} requests, {} round trips), recall {:.4}; \
         warm per query {:.1} requests {:.1} KiB {:.1} round trips, recall {:.4}; \
         steady (other queries) per query {:.1} requests {:.1} KiB {:.1} round trips, recall {:.4}",
        published.total as f64 / 1_048_576.0,
        published.sizes(),
        budget as f64 / 1_048_576.0,
        open_bytes as f64 / 1_048_576.0,
        cold.requests as f64 / queries.len() as f64,
        cold.bytes as f64 / queries.len() as f64 / 1024.0,
        cold.waves as f64 / queries.len() as f64,
        cold.worst_requests,
        cold.worst_waves,
        cold.recall,
        warm.requests as f64 / queries.len() as f64,
        warm.bytes as f64 / queries.len() as f64 / 1024.0,
        warm.waves as f64 / queries.len() as f64,
        warm.recall,
        steady.requests as f64 / fresh.len() as f64,
        steady.bytes as f64 / fresh.len() as f64 / 1024.0,
        steady.waves as f64 / fresh.len() as f64,
        steady.recall,
    );
    Report {
        name,
        total: published.total,
        open_requests,
        open_bytes,
        cold,
        warm,
        steady,
    }
}

#[test]
fn vector_indexes_are_served_by_range_cold_and_warm() {
    let n: usize = std::env::var("HSEARCH_VECTOR_RANGED_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    eprintln!(
        "demand paging available: {}",
        lucene_store::demand_paging_available()
    );
    let vectors = corpus(n, 7);
    let queries = corpus(50, 99);
    let fresh = corpus(50, 123);
    let budget = std::env::var("HSEARCH_VECTOR_RANGED_CACHE_MIB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(8)
        << 20;
    let hnsw = VectorSpec::hnsw(DIM as u16, Similarity::L2);
    let disk = VectorSpec::hnsw_disk(DIM as u16, Similarity::L2, CodeQuantizer::Bbq);
    let ivf = VectorSpec::ivf(
        DIM as u16,
        Similarity::L2,
        512,
        CodeQuantizer::Bbq,
        Rerank::F32,
    );
    let case = |name, warm, spec, effort| {
        measure(
            name,
            warm,
            spec,
            effort,
            &vectors,
            (&queries, &fresh),
            budget,
        )
    };
    let reports = [
        case("hnsw", false, hnsw, (200, 0)),
        case("hnsw-disk", false, disk, (400, 0)),
        case("ivf", false, ivf.with_nprobe(16), (100, 0)),
        case("ivf-warm-on-open", true, ivf.with_nprobe(16), (100, 0)),
    ];
    for r in &reports {
        assert!(r.cold.recall >= 0.90, "{}: {}", r.name, r.cold.recall);
        assert!(r.warm.recall >= 0.90, "{}: {}", r.name, r.warm.recall);
        assert!(r.steady.recall >= 0.90, "{}: {}", r.name, r.steady.recall);
        assert!(budget < r.total);
        // Re-running the same queries over a cache that holds their pages
        // costs no round trip, and other queries cost under one on average.
        assert!(r.warm.requests == 0, "{}: {}", r.name, r.warm.requests);
        assert!(r.steady.requests <= 50, "{}: {}", r.name, r.steady.requests);
        // Open issues a bounded number of reads, whatever the index size.
        assert!(r.open_requests <= 200, "{}: {}", r.name, r.open_requests);
        if r.name.ends_with("warm-on-open") {
            // The setting reads the vector files whole at open.
            assert!(r.open_bytes * 2 > r.total, "{}: {}", r.name, r.open_bytes);
        } else {
            // By default open reads headers and metadata only.
            assert!(r.open_bytes * 10 < r.total, "{}: {}", r.name, r.open_bytes);
        }
    }
    // A cluster probe is a range read and a rerank candidate a small one, so
    // IVF needs far fewer requests and round trips than the graph walks do.
    let by = |n: &str| reports.iter().find(|r| r.name == n).unwrap();
    assert!(by("ivf").cold.requests * 3 < by("hnsw").cold.requests);
    assert!(by("ivf").cold.waves * 3 < by("hnsw").cold.waves);
}
