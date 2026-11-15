//! Index one document with a text field of N MiB through `Node::bulk` on the
//! in-memory store and report time and peak memory, so memory per document can
//! be read against document size. One process per size: the high-water mark is
//! the process's. Provisional: one shared host.
//!
//!     cargo run --release -p hsearch-node --example large_field_profile -- [MiB] [docs]
use std::sync::Arc;
use std::time::Instant;

use hsearch_index::Wait;
use hsearch_node::{Limits, Node};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use serde_json::json;

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
    let mib: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let docs: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(1);
    let bytes = (mib * 1_048_576.0) as usize;
    let dir = std::env::temp_dir().join(format!("hsearch-largefield-{}", std::process::id()));
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
    let limits = Limits {
        max_document_bytes: 100 << 20,
        max_bulk_bytes: 128 << 20,
        max_bulk_items: 100,
        buffer_bytes: 16 << 20,
        memory_budget_bytes: 256 << 20,
    };
    let node = Node::with_limits(store, dir.clone(), limits).expect("node");
    let mapping = json!({"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"keyword"},
        "n":{"type":"integer"},"ok":{"type":"boolean"},"at":{"type":"date"}}}});
    node.create("orders", &mapping).expect("create");
    // Distinct words, so postings are not trivially one term.
    let mut title = String::with_capacity(bytes);
    let mut i = 0u64;
    while title.len() < bytes {
        title.push_str(&format!("w{} ", i % 5000));
        i += 1;
    }
    let baseline = status_kb("VmRSS:");
    let started = Instant::now();
    for d in 0..docs {
        let body = format!(
            "{{\"index\":{{\"_id\":\"d{d}\"}}}}\n{{\"title\":\"{title}\",\"tag\":\"a\",\"n\":1,\"ok\":true,\"at\":1}}\n"
        );
        let out = node
            .bulk(
                body.as_bytes(),
                Some(body.len() as u64),
                Some("orders"),
                Wait::Searchable,
            )
            .expect("bulk")
            .1;
        assert_eq!(out["errors"], json!(false), "{out}");
    }
    let secs = started.elapsed().as_secs_f64();
    let peak = status_kb("VmHWM:").saturating_sub(baseline);
    println!(
        "{docs} doc(s) of {:.2} MiB: {secs:.2}s, {:.1} MiB/s, peak RSS over baseline {:.1} MiB ({:.1}x the document)",
        title.len() as f64 / 1_048_576.0,
        docs as f64 * title.len() as f64 / 1_048_576.0 / secs,
        peak as f64 / 1024.0,
        peak as f64 / 1024.0 / (title.len() as f64 / 1_048_576.0),
    );
    let _ = std::fs::remove_dir_all(dir);
}
