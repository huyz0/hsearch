//! Merge time and memory of one vector variant: `SEGMENTS` segments of
//! `PER_SEGMENT` vectors are written, then merged into one, and the merged
//! index is searched. One process per variant, so peak RSS is that variant's.
//! Provisional: one shared VM, no ARM.
//!
//!     cargo run --release -p hsearch-engine --example vector_merge_bench -- \
//!         [plain|bbq|ivf-bbq|ivf-osq4|disk-osq4] [PER_SEGMENT] [SEGMENTS] [DIM]
//!
//! IVF clusters at 512 vectors; queries are drawn like the data and run at the
//! calibrated depth (IVF) or `HS_BEAM` (default 200) candidates (HNSW).
//! `HS_MERGE=0` skips the merge and searches the flushed segments, for the
//! unmerged comparison. The peak RSS column is the process high-water mark
//! minus the RSS after the writes, so it is an upper bound on the merge's.
use hsearch_engine::{
    CodeQuantizer, Directory, Engine, EngineDirectory, Kind, Limits, Mutation, Quantizer, Query,
    Rerank, Schema, Similarity, Sort, Value, VectorSpec,
};
use lucene_store::ByteBuffersDirectory;
use std::time::Instant;

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

fn corpus(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    corpus_at(n, dim, (seed, seed))
}

/// `n` vectors around the 64 centres `centre_seed` draws, with noise from
/// `noise_seed`: the same centres as the data make in-distribution queries.
fn corpus_at(n: usize, dim: usize, (centre_seed, noise_seed): (u64, u64)) -> Vec<Vec<f32>> {
    let mut rng = Rng(centre_seed);
    let centres: Vec<Vec<f32>> = (0..64)
        .map(|_| (0..dim).map(|_| rng.normal() * 3.0).collect())
        .collect();
    let mut rng = Rng(noise_seed);
    (0..n)
        .map(|i| centres[i % 64].iter().map(|c| c + rng.normal()).collect())
        .collect()
}

fn status_kb(field: &str) -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let variant = args.next().unwrap_or_else(|| "ivf-bbq".into());
    let per: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(25_000);
    let segments: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(4);
    let dim: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(128);
    let beam: usize = std::env::var("HS_BEAM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let n = per * segments;
    let d = dim as u16;
    let spec = match variant.as_str() {
        "plain" => VectorSpec::hnsw(d, Similarity::L2),
        "bbq" => VectorSpec::hnsw(d, Similarity::L2).with_quantizer(Quantizer::Bbq),
        "ivf-bbq" => VectorSpec::ivf(d, Similarity::L2, 512, CodeQuantizer::Bbq, Rerank::F32),
        "ivf-osq4" => VectorSpec::ivf(d, Similarity::L2, 512, CodeQuantizer::Osq(4), Rerank::F32),
        "disk-osq4" => VectorSpec::hnsw_disk(d, Similarity::L2, CodeQuantizer::Osq(4)),
        other => panic!("unknown variant {other}"),
    };
    let vectors = corpus(n, dim, 7);
    let queries = corpus_at(100, dim, (7, 99));
    let schema = Schema::new(1, &[("embedding", Kind::Vector(spec))])
        .unwrap()
        .with_limits(Limits::scale().with_document_bytes(dim * 4 + 1024))
        .unwrap();
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    let baseline = status_kb("VmRSS:");

    let started = Instant::now();
    for (s, segment) in vectors.chunks(per).enumerate() {
        for (b, chunk) in segment.chunks(16).enumerate() {
            let first = s * per + b * 16;
            let ids: Vec<String> = (0..chunk.len())
                .map(|j| format!("d{}", first + j))
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
                .apply(schema.prepare(&format!("b{first}"), &mutations).unwrap())
                .unwrap();
        }
        drop(engine.refresh().unwrap());
    }
    let write = started.elapsed().as_secs_f64();
    let size = |directory: &ByteBuffersDirectory| {
        directory
            .list_all()
            .unwrap()
            .iter()
            .map(|name| directory.file_length(name).unwrap())
            .sum::<u64>()
    };
    let before = size(&directory);

    let count = |directory: &ByteBuffersDirectory| {
        directory
            .list_all()
            .unwrap()
            .iter()
            .filter(|name| name.ends_with(".si"))
            .count()
    };
    let segments_before = count(&directory);
    let rss_before = status_kb("VmRSS:");
    engine.commit().unwrap();
    let started = Instant::now();
    if std::env::var("HS_MERGE").is_ok_and(|v| v == "0") {
        eprintln!("HS_MERGE=0: searching the flushed segments unmerged");
    } else {
        engine.merge().unwrap();
    }
    let snapshot = engine.refresh().unwrap();
    let merge = started.elapsed().as_secs_f64();
    let peak_over = status_kb("VmHWM:").saturating_sub(rss_before) as f64 / 1024.0;
    let after = size(&directory);

    let mut found = 0;
    let mut latencies = Vec::new();
    for q in &queries {
        let started = Instant::now();
        let hits = snapshot
            .search(
                &Query::Knn {
                    field: "embedding",
                    vector: q,
                    k: 10,
                    candidates: beam,
                    nprobe: 0,
                    filter: None,
                },
                &[Sort::Score],
                10,
                &[],
            )
            .unwrap()
            .hits;
        latencies.push(started.elapsed().as_secs_f64() * 1e3);
        let mut exact: Vec<(f32, usize)> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| (v.iter().zip(q).map(|(a, b)| (a - b) * (a - b)).sum(), i))
            .collect();
        exact.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for (_, i) in exact.iter().take(10) {
            found += hits.iter().filter(|h| h.id == format!("d{i}")).count();
        }
    }
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{variant} {segments} x {per} d={dim} beam {beam} ({segments_before} flushed, {} after): write {write:.1}s ({:.0} vectors/s), merge {merge:.1}s ({:.0} vectors/s), \
         peak RSS over pre-merge {peak_over:.0} MiB (baseline {:.0} MiB), index {:.1} MiB before, {:.1} MiB after, \
         merged query p50 {:.2} ms, recall@10 {:.4}",
        count(&directory),
        n as f64 / write,
        n as f64 / merge,
        (rss_before - baseline) as f64 / 1024.0,
        before as f64 / 1_048_576.0,
        after as f64 / 1_048_576.0,
        latencies[latencies.len() / 2],
        found as f64 / (10 * queries.len()) as f64,
    );
}
