//! Build time, index size, memory, query latency and recall@10 of one vector
//! variant on a synthetic clustered corpus. One process per variant, so the
//! peak RSS is that variant's. Provisional: one shared VM, no ARM.
//!
//!     cargo run --release -p hsearch-engine --example vector_bench -- \
//!         [plain|scalar8|scalar4|scalar1|bbq|rabitq|erq4|ivf-bbq|ivf-osq4|ivf-erq4|ivf-auto|ivf-bbq-f16] \
//!         [N] [DIM] [BEAM] [NPROBE]
//!
//! An `ivf-` variant clusters at 512 vectors, searches `NPROBE` clusters
//! (0 is the automatic depth) and reranks `BEAM` candidates.
use std::time::Instant;

use hsearch_engine::{
    Directory, Engine, EngineDirectory, IvfQuantizer, Kind, Limits, Mutation, Quantizer, Query,
    Rerank, ScalarBits, Schema, Similarity, Sort, Value, VectorSpec,
};
use lucene_store::ByteBuffersDirectory;

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
    let mut rng = Rng(seed);
    let centres: Vec<Vec<f32>> = (0..64)
        .map(|_| (0..dim).map(|_| rng.normal() * 3.0).collect())
        .collect();
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
    let variant = args.next().unwrap_or_else(|| "plain".into());
    let n: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(20_000);
    let dim: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(128);
    let beam: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(100);
    let nprobe: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let ivf = |q, r| VectorSpec::ivf(dim as u16, Similarity::L2, 512, q, r);
    let ivf_spec = match variant.as_str() {
        "ivf-bbq" => Some(ivf(IvfQuantizer::Bbq, Rerank::F32)),
        "ivf-osq4" => Some(ivf(IvfQuantizer::Osq(4), Rerank::F32)),
        "ivf-erq4" => Some(ivf(IvfQuantizer::Erq(4), Rerank::F32)),
        "ivf-auto" => Some(ivf(IvfQuantizer::Auto, Rerank::F32)),
        "ivf-bbq-f16" => Some(ivf(IvfQuantizer::Bbq, Rerank::F16)),
        _ => None,
    };
    let quantizer = match variant.as_str() {
        v if v.starts_with("ivf-") => Quantizer::None,
        "plain" => Quantizer::None,
        "scalar8" => Quantizer::Scalar(ScalarBits::Eight),
        "scalar4" => Quantizer::Scalar(ScalarBits::Four),
        "scalar1" => Quantizer::Scalar(ScalarBits::One),
        "bbq" => Quantizer::Bbq,
        "rabitq" => Quantizer::RaBitQ,
        "erq4" => Quantizer::Erq(4),
        other => panic!("unknown variant {other}"),
    };
    let vectors = corpus(n, dim, 7);
    let queries = corpus(200, dim, 99);
    let spec = ivf_spec
        .unwrap_or_else(|| VectorSpec::hnsw(dim as u16, Similarity::L2).with_quantizer(quantizer));
    let schema = Schema::new(1, &[("embedding", Kind::Vector(spec))])
        .unwrap()
        .with_limits(Limits::scale().with_document_bytes(dim * 4 + 1024))
        .unwrap();
    let directory = ByteBuffersDirectory::new();
    let observed = EngineDirectory::new(&directory);
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    let baseline = status_kb("VmRSS:");

    let started = Instant::now();
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
    let snapshot = engine.refresh().unwrap();
    let build = started.elapsed().as_secs_f64();
    let peak_mib = (status_kb("VmHWM:").saturating_sub(baseline)) as f64 / 1024.0;
    let mut bytes = 0u64;
    for name in directory.list_all().unwrap() {
        bytes += directory.file_length(&name).unwrap();
    }

    let mut latencies = Vec::new();
    let mut found = 0;
    for q in &queries {
        let started = Instant::now();
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
        "{variant} n={n} dim={dim} beam={beam} nprobe={nprobe}: build {build:.2}s, index {:.1} MiB ({:.0} B/vector), \
         peak RSS over baseline {peak_mib:.0} MiB, query p50 {:.2} ms p99 {:.2} ms, recall@10 {:.4}",
        bytes as f64 / 1_048_576.0,
        bytes as f64 / n as f64,
        latencies[latencies.len() / 2],
        latencies[latencies.len() * 99 / 100],
        found as f64 / (10 * queries.len()) as f64,
    );
}
