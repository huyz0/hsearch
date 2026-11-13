//! M02.03 benchmark: `RangeCache` over an in-memory source, so the numbers are
//! the cache's own cost (lookup, copies, eviction) with the network removed.
//! Run `cargo run --release -p hsearch-engine --example range_bench`.
use hsearch_engine::{RangeCache, RangeFiles, RangeLimits, RemoteError};
use std::time::Instant;

struct Memory(Vec<u8>);
impl RangeFiles for Memory {
    fn read_at(&self, _name: &str, offset: u64, buf: &mut [u8]) -> Result<(), RemoteError> {
        let start = usize::try_from(offset).map_err(|_| RemoteError::Unavailable)?;
        buf.copy_from_slice(
            self.0
                .get(start..start + buf.len())
                .ok_or(RemoteError::Unavailable)?,
        );
        Ok(())
    }
}

fn limits(page: usize, cache: u64, run: usize, trace: usize) -> RangeLimits {
    RangeLimits {
        page_bytes: page,
        cache_bytes: cache,
        max_run_pages: run,
        trace_events: trace,
    }
}

fn main() {
    const FILE: usize = 256 << 20;
    let data: Vec<u8> = (0..FILE)
        .map(|i| (i.wrapping_mul(2654435761) >> 7) as u8)
        .collect();
    let manifest = vec![("f".to_owned(), FILE as u64)];
    println!("file {} MiB, in-memory source (no network)", FILE >> 20);
    for (page, run) in [(4 << 10, 16), (64 << 10, 16), (64 << 10, 4), (1 << 20, 4)] {
        let cache_bytes = 32u64 << 20; // 1/8 of the file
        let cache = RangeCache::new(
            Memory(data.clone()),
            manifest.clone(),
            limits(page, cache_bytes, run, 0),
        )
        .unwrap();
        let input = cache.input("f").unwrap();
        // Cold sequential scan, 1 MiB reads (8x the cache in total).
        let mut buf = vec![0u8; 1 << 20];
        let start = Instant::now();
        for at in (0..FILE).step_by(buf.len()) {
            input.read_at(at as u64, &mut buf).unwrap();
        }
        let scan = start.elapsed();
        let s = cache.stats();
        println!(
            "page {:>7} run {:>2}: scan {:>6.0} MB/s, {} GETs ({} B/GET), bound {} KiB",
            page,
            run,
            FILE as f64 / scan.as_secs_f64() / 1e6,
            s.gets,
            s.get_bytes / s.gets.max(1),
            cache.memory_bound_bytes() >> 10
        );
        // Random 4 KiB reads over the whole file: working set 8x the cache.
        let mut small = [0u8; 4096];
        let mut state = 99u64;
        let before = cache.stats();
        let reads = 200_000u64;
        let start = Instant::now();
        for _ in 0..reads {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let at = (state >> 20) % (FILE as u64 - 4096);
            input.read_at(at, &mut small).unwrap();
        }
        let random = start.elapsed();
        let after = cache.stats();
        println!(
            "      random 4 KiB x{reads}: {:>6.0} ns/read, hit rate {:.1}%, {} GETs ({:.2} GET/read), {:.1}x bytes read amplification",
            random.as_nanos() as f64 / reads as f64,
            100.0 * (after.hits - before.hits) as f64
                / ((after.hits - before.hits) + (after.misses - before.misses)) as f64,
            after.gets - before.gets,
            (after.gets - before.gets) as f64 / reads as f64,
            (after.get_bytes - before.get_bytes) as f64 / (reads * 4096) as f64
        );
        // Warm: a 4 MiB hot set that fits, 4 KiB reads.
        let hot = 4u64 << 20;
        for at in (0..hot).step_by(4096) {
            input.read_at(at, &mut small).unwrap();
        }
        let start = Instant::now();
        for i in 0..reads {
            let at = (i * 4096) % hot;
            input.read_at(at, &mut small).unwrap();
        }
        let warm = start.elapsed();
        println!(
            "      warm hot-set 4 KiB x{reads}: {:>6.0} ns/read, {:.1} GB/s",
            warm.as_nanos() as f64 / reads as f64,
            reads as f64 * 4096.0 / warm.as_secs_f64() / 1e9
        );
    }
    // Trace overhead on the warm path.
    for trace in [0usize, 4096, 2_100_000] {
        let cache = RangeCache::new(
            Memory(data[..8 << 20].to_vec()),
            vec![("f".to_owned(), 8 << 20)],
            limits(64 << 10, 16 << 20, 4, trace),
        )
        .unwrap();
        let input = cache.input("f").unwrap();
        let mut small = [0u8; 4096];
        for at in (0..8u64 << 20).step_by(4096) {
            input.read_at(at, &mut small).unwrap();
        }
        let start = Instant::now();
        for i in 0..1_000_000u64 {
            input.read_at((i * 4096) % (8 << 20), &mut small).unwrap();
        }
        println!(
            "trace_events {trace:>4}: warm {:.0} ns/read",
            start.elapsed().as_nanos() as f64 / 1e6
        );
    }
}
