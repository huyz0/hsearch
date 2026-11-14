//! Call cost of the native loader (M24.02): latency, throughput and memory.
//!
//! `cargo run --release -p hsearch-module-native --example bench`
//!
//! Builds `tests/c/hs_test_module.c` with `cc`, loads it through `NativeModule`, and
//! times the echo point at three payload sizes, then runs the same function directly
//! from C (`examples/direct.c`) as the floor. Allocation counts per call are taken
//! separately with valgrind (see the evidence file), because counting them in-process
//! needs a custom global allocator, which needs `unsafe` outside the loader's `sys`
//! module. Numbers measured on a shared cloud VM are **provisional** and relative.

use hsearch_module_abi::Kind;
use hsearch_module_native::{LoadConfig, NativeModule, Request};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const DESC: &str = r#"{"name":"bench","version":"0.1.0","abi":"1.0","engine_api":"1.0","max_concurrent_calls":8,"io_bytes":4096,"extension_points":[{"kind":"analyzer","id":"echo","version":"1.0.0","deterministic":true}]}"#;

fn cc(args: &[&str], out: &Path) {
    let ok = Command::new("cc")
        .args(args)
        .arg("-o")
        .arg(out)
        .status()
        .expect("cc")
        .success();
    assert!(ok, "cc failed");
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir: PathBuf = std::env::temp_dir().join("hsearch-native-bench");
    std::fs::create_dir_all(&dir).expect("dir");
    let header = root.join("../../fixtures/extensions/abi-v1");
    let so = dir.join("module.so");
    let define = format!("-DDESC_JSON=\"{}\"", DESC.replace('"', "\\\""));
    let header_flag = format!("-I{}", header.display());
    cc(
        &[
            "-shared",
            "-fPIC",
            "-O2",
            &header_flag,
            &define,
            root.join("tests/c/hs_test_module.c")
                .to_str()
                .expect("path"),
        ],
        &so,
    );
    let direct = dir.join("direct");
    cc(
        &[
            "-O2",
            &header_flag,
            root.join("examples/direct.c").to_str().expect("path"),
            "-ldl",
        ],
        &direct,
    );
    let sha: [u8; 32] = Sha256::digest(std::fs::read(&so).expect("so")).into();

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("alloc") {
        // `bench alloc N`: N calls and exit, for `valgrind` to count allocations (run it twice, diff).
        let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1000);
        let module = NativeModule::load(&so, LoadConfig::new(sha)).expect("load");
        let mut input = vec![b'e'];
        input.extend(std::iter::repeat_n(b'a', 4096));
        let request = Request {
            kind: Kind::Analyzer,
            point: "echo",
            input: &input,
            timeout: Duration::from_secs(5),
            output_bound: 4096,
            output_limit: 4096,
        };
        let mut out = Vec::with_capacity(4096);
        for _ in 0..n {
            module.call(&request, &mut out).expect("call");
        }
        let _ = module.close(Duration::from_secs(2));
        return;
    }
    let iters: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let started = Instant::now();
    let module = NativeModule::load(&so, LoadConfig::new(sha)).expect("load");
    println!(
        "load: {:?} (hash, sealed copy, dlopen, init, describe, 8 workers)",
        started.elapsed()
    );
    println!(
        "cpus: {}",
        std::thread::available_parallelism().map_or(0, usize::from)
    );
    println!(
        "\nsingle caller, echo, {iters} calls each (ns per call; the timer itself costs ~25 ns)"
    );
    println!(
        "{:>10} {:>10} {:>10} {:>10} {:>14}",
        "payload", "p50", "p99", "direct p50", "pooled/direct"
    );
    for payload in [64usize, 4096, 65536] {
        let mut input = vec![b'e'];
        input.extend(std::iter::repeat_n(b'a', payload));
        let request = Request {
            kind: Kind::Analyzer,
            point: "echo",
            input: &input,
            timeout: Duration::from_secs(5),
            output_bound: payload as u32,
            output_limit: payload as u32,
        };
        let mut out = Vec::with_capacity(payload);
        for _ in 0..2000 {
            module.call(&request, &mut out).expect("warm");
        }
        let mut times = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            module.call(&request, &mut out).expect("call");
            times.push(t.elapsed().as_nanos() as u64);
        }
        times.sort_unstable();
        let floor = Command::new(&direct)
            .arg(&so)
            .arg(payload.to_string())
            .arg(iters.to_string())
            .output()
            .expect("direct");
        let floor = String::from_utf8_lossy(&floor.stdout);
        let direct_p50: f64 = floor
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(f64::NAN);
        let p50 = percentile(&times, 0.5);
        println!(
            "{:>10} {:>10} {:>10} {:>10.0} {:>13.1}x",
            payload,
            p50,
            percentile(&times, 0.99),
            direct_p50,
            p50 as f64 / direct_p50
        );
    }

    println!("\nthroughput, echo 4 KiB, callers x calls each (module has 8 workers)");
    println!(
        "{:>8} {:>12} {:>14}",
        "callers", "calls/s", "ns/call/caller"
    );
    for callers in [1usize, 2, 4, 8, 16] {
        let per = 50_000usize;
        let mut input = vec![b'e'];
        input.extend(std::iter::repeat_n(b'a', 4096));
        let began = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..callers {
                s.spawn(|| {
                    let request = Request {
                        kind: Kind::Analyzer,
                        point: "echo",
                        input: &input,
                        timeout: Duration::from_secs(5),
                        output_bound: 4096,
                        output_limit: 4096,
                    };
                    let mut out = Vec::with_capacity(4096);
                    for _ in 0..per {
                        // Busy only appears past 3x workers in flight; 16 callers is within it.
                        module.call(&request, &mut out).expect("call");
                    }
                });
            }
        });
        let secs = began.elapsed().as_secs_f64();
        println!(
            "{callers:>8} {:>12.0} {:>14.0}",
            (callers * per) as f64 / secs,
            secs * 1e9 / per as f64
        );
    }
    let rss = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    println!(
        "\n{}",
        rss.lines()
            .filter(|l| l.starts_with("VmRSS") || l.starts_with("VmHWM"))
            .collect::<Vec<_>>()
            .join("  ")
    );
    let _ = module.close(Duration::from_secs(2));
}
