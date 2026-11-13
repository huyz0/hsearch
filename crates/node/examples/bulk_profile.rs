//! Time `Node::bulk` for N documents on the in-memory store, so the numbers are the
//! node's and the engine's own cost with no network. Use with `valgrind --tool=callgrind`
//! for a function-level profile. Provisional: one shared host.
//!
//!     cargo run --release -p hsearch-node --example bulk_profile -- [docs] [batch]
use std::sync::Arc;
use std::time::Instant;

use hsearch_index::Wait;
use hsearch_node::Node;
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use serde_json::json;

fn main() {
    let mut args = std::env::args().skip(1);
    let docs: usize = args
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000)
        .clamp(1, 5_000_000);
    let batch: usize = args
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000)
        .clamp(1, 50_000);
    let dir = std::env::temp_dir().join(format!("hsearch-profile-{}", std::process::id()));
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
    let node = Node::new(store, dir.clone());
    let mapping = json!({"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"keyword"},
        "n":{"type":"integer"},"ok":{"type":"boolean"},"at":{"type":"date"}}}});
    node.create("orders", &mapping).expect("create");
    let started = Instant::now();
    let mut worst = 0.0f64;
    for start in (0..docs).step_by(batch) {
        let body: String = (start..(start + batch).min(docs))
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
            .bulk(body.as_bytes(), Some("orders"), Wait::Searchable)
            .expect("bulk");
        assert_eq!(out["errors"], json!(false));
        worst = worst.max(t.elapsed().as_secs_f64() * 1000.0);
    }
    let secs = started.elapsed().as_secs_f64();
    println!(
        "{docs} docs, batch {batch}: {secs:.2}s, {:.0} docs/s, slowest bulk {worst:.0} ms",
        docs as f64 / secs
    );
    let _ = std::fs::remove_dir_all(dir);
}
