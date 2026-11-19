//! Measurements for M21.05, not checks. Run:
//! `cargo test --release -p hsearch-index --test realtime_bench -- --ignored --nocapture`
mod support;

use hsearch_engine::Limits;
use hsearch_index::{Index, Wait};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use std::sync::Arc;
use std::time::Instant;
use support::*;

fn open(label: &str) -> (Index, Scratch) {
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
    let scratch = Scratch::new(label);
    let schema = schema().with_limits(Limits::scale()).unwrap();
    (
        Index::create(store, "b", schema, &scratch.0).unwrap(),
        scratch,
    )
}

/// Durable write cost per operation, in batches of 256 (the write path's hot loop).
#[test]
#[ignore = "a measurement, not a check"]
fn write_cost() {
    for round in 0..3 {
        let (index, _s) = open("bench-write");
        let rounds = 40;
        let started = Instant::now();
        for r in 0..rounds {
            let ops = (0..256)
                .map(|i| put(&format!("{}", r * 256 + i), "tag", i as i64))
                .collect();
            index.bulk(ops, Wait::Durable).unwrap();
        }
        let per = started.elapsed().as_nanos() as f64 / (rounds * 256) as f64;
        println!("round {round}: durable write {per:.0} ns/op");
    }
}

/// Read cost: a document only the owner's record holds, one the snapshot holds,
/// and a multi-get of 10, 20 and 100 of each.
#[test]
#[ignore = "a measurement, not a check"]
fn read_cost() {
    let (index, _s) = open("bench-read");
    let ops = (0..3000).map(|i| put(&format!("s{i}"), "tag", i)).collect();
    index.bulk(ops, Wait::Searchable).unwrap();
    let ops = (0..3000).map(|i| put(&format!("r{i}"), "tag", i)).collect();
    index.bulk(ops, Wait::Durable).unwrap();
    let projection = ["tag", "n"];
    for (label, prefix, realtime) in [
        ("owner record", "r", true),
        ("snapshot (realtime)", "s", true),
        ("snapshot (realtime=false)", "s", false),
    ] {
        for count in [1usize, 10, 20, 100] {
            let ids: Vec<String> = (0..count).map(|i| format!("{prefix}{}", i * 7)).collect();
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            let rounds = 20_000 / count;
            let started = Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(index.get_many(&refs, &projection, realtime).unwrap());
            }
            let per_call = started.elapsed().as_nanos() as f64 / rounds as f64;
            println!(
                "{label:28} {count:3} ids: {per_call:9.0} ns/call, {:7.0} ns/doc",
                per_call / count as f64
            );
        }
    }
}
