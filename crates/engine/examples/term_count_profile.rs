//! Peak memory and time of counting a large text field's tokens for the term
//! bound: `materialized` collects every token (the previous behaviour),
//! `streamed` counts them as the analyzer produces them (the current one).
//! One process per mode: the high-water mark is the process's. Provisional.
//!
//!     cargo run --release -p hsearch-engine --example term_count_profile -- [streamed|materialized] [MiB]
use std::time::Instant;

use lucene_analysis::{Analyzer, StandardAnalyzer};

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
    let mode = args.next().unwrap_or_else(|| "streamed".into());
    let mib: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(4);
    let mut text = String::with_capacity(mib << 20);
    let mut i = 0u64;
    while text.len() < mib << 20 {
        text.push_str(&format!("w{} ", i % 5000));
        i += 1;
    }
    let analyzer = Analyzer::new(StandardAnalyzer::new());
    let baseline = status_kb("VmRSS:");
    let started = Instant::now();
    let count = if mode == "materialized" {
        analyzer
            .try_analyze_stream(&text)
            .expect("analysis")
            .tokens
            .len()
    } else {
        let mut count = 0usize;
        analyzer
            .try_for_each_token(&text, |_, _, _, _| count += 1)
            .expect("analysis");
        count
    };
    println!(
        "{mode}: {count} tokens in {:.3}s, peak RSS over baseline {:.1} MiB",
        started.elapsed().as_secs_f64(),
        status_kb("VmHWM:").saturating_sub(baseline) as f64 / 1024.0
    );
}
