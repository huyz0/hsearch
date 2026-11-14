//! Cost of the WASM host: load, one call at several sizes, and memory per instance.
//! `cargo run --release -p hsearch-module-wasm --example bench`. Numbers from a VM are provisional.

use hsearch_module_abi::Kind;
use hsearch_module_wasm::{Limits, Request, WasmConfig, WasmModule};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

fn rss_kib() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

fn config(bytes: &[u8]) -> WasmConfig {
    let mut c = WasmConfig::new(Sha256::digest(bytes).into());
    c.limits = Limits {
        max_memory_bytes: 3 * 65536,
        ..Limits::default()
    };
    c
}

fn main() {
    let bytes = wat::parse_str(include_str!("lower.wat")).expect("wat");
    let t = Instant::now();
    let m = WasmModule::load_bytes(&bytes, config(&bytes)).expect("load");
    println!(
        "load (compile, validate, first instance, describe, init): {:?}",
        t.elapsed()
    );
    let t = Instant::now();
    for _ in 0..20 {
        drop(WasmModule::load_bytes(&bytes, config(&bytes)).expect("load"));
    }
    println!(
        "load, mean of 20 (each builds an engine): {:?}",
        t.elapsed() / 20
    );
    let mut out = Vec::new();
    for size in [64usize, 4096, 65_000] {
        let input: Vec<u8> = (0..size).map(|i| b'A' + (i % 26) as u8).collect();
        let req = Request {
            kind: Kind::Analyzer,
            point: "lowercase_ascii",
            input: &input,
            timeout: Duration::from_secs(5),
            output_bound: 65_536,
            output_limit: 65_536,
        };
        for _ in 0..2000 {
            m.call(&req, &mut out).expect("warm");
        }
        let iters = if size > 10_000 { 20_000 } else { 100_000 };
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            m.call(&req, &mut out).expect("call");
            samples.push(t.elapsed().as_nanos() as u64);
        }
        samples.sort_unstable();
        let p = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
        assert_eq!(out.len(), size);
        println!(
            "call {size:>6} B: p50 {:>8} ns  p99 {:>8} ns  ({:.2} ns/byte at p50)",
            p(0.5),
            p(0.99),
            p(0.5) as f64 / size as f64
        );
    }
    // Memory: instances are created lazily up to the descriptor's concurrency (2 here).
    let before = rss_kib();
    let held: Vec<_> = (0..10)
        .map(|_| WasmModule::load_bytes(&bytes, config(&bytes)).expect("load"))
        .collect();
    let after = rss_kib();
    println!(
        "resident per loaded module with one instance: {} KiB (10 modules, {} KiB total)",
        (after.saturating_sub(before)) / 10,
        after.saturating_sub(before)
    );
    drop(held);
}
