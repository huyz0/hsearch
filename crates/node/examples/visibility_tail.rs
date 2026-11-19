//! Per-call latency of `Node::bulk` with `Wait::Searchable`, batch by batch, on the
//! in-memory store (no network), printing every call slower than a threshold with its
//! index and the documents already written, so a recurring tail can be located.
//! Provisional: one shared host.
//!
//!     cargo run --release -p hsearch-node --example visibility_tail -- [batches] [batch] [slow_ms] [auto_merge_mb] [background: 0|1] [store: memory|s3]
//!
//! With `s3` the catalog lives on the RustFS from `scripts/rustfs.sh up` (`HSEARCH_S3_*`
//! exported) and the S3 request counts are printed; segments stay on the local disk.
use std::sync::Arc;
use std::time::{Duration, Instant};

use hsearch_index::Wait;
use hsearch_node::{Limits, Node};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{Budget, Cancellation, MetadataStore, RequestContext};
use hsearch_s3::{S3Config, S3Store};
use serde_json::json;

fn written_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("wchar:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn s3_store() -> Arc<S3Store> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let store = S3Store::new(S3Config {
        endpoint: std::env::var("HSEARCH_S3_ENDPOINT").expect("HSEARCH_S3_ENDPOINT"),
        bucket: format!("hvt-{nanos}"),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
    .unwrap();
    let ctx = RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 30,
            response_bytes: 1 << 30,
            attempts: 100_000,
        },
    )
    .unwrap();
    store.prepare_bucket(&ctx).expect("prepare bucket");
    Arc::new(store)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.get(5).is_some_and(|v| v == "s3") {
        let store = s3_store();
        drive(Arc::clone(&store), &args);
        let c = store.counts();
        println!(
            "S3 requests: GET {} PUT {} HEAD {} LIST {} DELETE {}",
            c.get, c.put, c.head, c.list, c.delete
        );
    } else {
        let store = Arc::new(
            MemoryStore::new(MemoryLimits {
                objects: 4096,
                versions: 16384,
                version_bytes: 64 << 20,
                identities: 16384,
                identity_bytes: 64 << 20,
            })
            .unwrap(),
        );
        drive(store, &args);
    }
}

fn drive<S: MetadataStore + 'static>(store: Arc<S>, args: &[String]) {
    let mut args = args.iter().cloned();
    let batches: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(400);
    let batch: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(100);
    let slow: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(100.0);
    let auto_merge: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(4);
    let background = args.next().is_some_and(|v| v == "1");
    let dir = std::env::temp_dir().join(format!("hsearch-tail-{}", std::process::id()));
    let node = Node::with_limits(
        store,
        dir.clone(),
        Limits {
            auto_merge_mb: auto_merge,
            ..Limits::default()
        },
    )
    .expect("node")
    .with_background_merge(background);
    let mapping = json!({"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"keyword"},
        "n":{"type":"integer"},"ok":{"type":"boolean"},"at":{"type":"date"}}}});
    node.create("orders", &mapping).expect("create");
    let written0 = written_bytes();
    let started = Instant::now();
    let mut times = Vec::with_capacity(batches);
    for b in 0..batches {
        let body: String = (b * batch..(b + 1) * batch)
            .map(|i| {
                format!(
                    "{{\"index\":{{\"_id\":\"d{i}\"}}}}\n{{\"title\":\"quick fox item{i}\",\"tag\":\"t{}\",\"n\":{i},\"ok\":{},\"at\":{}}}\n",
                    i % 3,
                    i % 2 == 0,
                    1_700_000_000_000_i64 + i as i64
                )
            })
            .collect();
        let t = Instant::now();
        let out = node
            .bulk(
                body.as_bytes(),
                Some(body.len() as u64),
                Some("orders"),
                Wait::Searchable,
            )
            .expect("bulk")
            .1;
        assert_eq!(out["errors"], json!(false));
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if ms >= slow {
            println!("slow call {b} (docs before: {}): {ms:.1} ms", b * batch);
        }
        times.push(ms);
    }
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "n={} p50 {:.1} p99 {:.1} max {:.1} ms",
        sorted.len(),
        sorted[sorted.len() / 2],
        sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)],
        sorted[sorted.len() - 1]
    );
    let mut search = Vec::new();
    for _ in 0..200 {
        let t = Instant::now();
        node.search(
            "orders",
            &json!({"query": {"term": {"tag": "t1"}}, "size": 10}),
        )
        .unwrap();
        search.push(t.elapsed().as_secs_f64() * 1e3);
    }
    search.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "search after the run: p50 {:.2} ms p99 {:.2} ms",
        search[100], search[198]
    );
    println!(
        "total {:.1} s, process bytes written {:.0} MiB (wchar: disk writes, plus socket sends with the s3 store)",
        started.elapsed().as_secs_f64(),
        (written_bytes() - written0) as f64 / 1_048_576.0
    );
    let _ = std::fs::remove_dir_all(dir);
}
