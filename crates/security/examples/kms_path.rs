//! KMS path accounting (M05.08): `cargo run --release -p hsearch-security --features test-kms --example kms_path`.
//!
//! Runs each request path through the key manager over a [`MeteredProvider`] and prints, per path, the
//! provider calls made, the latency histogram summary and the price at the M02.07 rate card. The provider
//! here is the in-memory test provider with an injected latency, so **call counts are real for the
//! contract and every latency and price below is emulated, never a KMS measurement**. To run against a real
//! provider, replace `provider()` with an adapter (M15) and keep the frozen envelope: every path's event
//! count is fixed in this file and the run aborts if a path exceeds `MAX_CALLS` provider calls or the whole run
//! exceeds `MAX_RUN`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hsearch_security::audit::MemoryAuditSink;
use hsearch_security::clock::ManualClock;
use hsearch_security::ids::IndexId;
use hsearch_security::keys::{CallOptions, KeyContext, KeyProvider, KeyRef, Profile};
use hsearch_security::manager::{CachePolicy, KeyAllowlist, KeyManager};
use hsearch_security::metered::{MeteredProvider, Op};
use hsearch_security::test_kms::TestKms;

/// The frozen envelope for a real run.
const MAX_CALLS: u64 = 20_000;
const MAX_RUN: Duration = Duration::from_secs(600);
/// The M02.07 rate card: USD 0.003 per 1000 requests.
const USD_PER_CALL: f64 = 0.003 / 1000.0;
/// Emulated provider latency, applied to every call. Not a measurement.
const EMULATED_LATENCY: Duration = Duration::from_millis(5);

static RUN_STARTED: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

fn key() -> KeyRef {
    KeyRef::new("test/acct/default").unwrap()
}

fn ctx(n: u32) -> KeyContext {
    let mut object = [0u8; 16];
    object[..4].copy_from_slice(&n.to_le_bytes());
    KeyContext {
        index: IndexId::new("orders").unwrap(),
        domain: [7; 16],
        object,
    }
}

fn provider() -> (Arc<TestKms>, Arc<MeteredProvider>) {
    let kms = Arc::new(TestKms::new());
    kms.create_key(&key()).unwrap();
    kms.set_latency(EMULATED_LATENCY);
    let inner: Arc<dyn KeyProvider> = kms.clone();
    (kms, Arc::new(MeteredProvider::new(inner)))
}

fn manager(meter: &Arc<MeteredProvider>, clock: &Arc<ManualClock>, ttl: Duration) -> KeyManager {
    KeyManager::new(
        Profile::Development,
        meter.clone(),
        KeyAllowlist::new(vec![("test".into(), "acct".into())]),
        key(),
        CachePolicy {
            max_entries: 100_000,
            ttl,
            shards: 8,
        },
        Arc::new(MemoryAuditSink::new(1_000_000)),
        clock.clone(),
    )
    .unwrap()
}

fn report(path: &str, events: u64, meter: &MeteredProvider, started: Instant) {
    let s = meter.snapshot();
    let calls = s.all();
    assert!(calls <= MAX_CALLS, "envelope exceeded: {calls} calls");
    assert!(
        RUN_STARTED.elapsed() <= MAX_RUN,
        "envelope exceeded: run time"
    );
    let (p50, p99) = [Op::Wrap, Op::Unwrap]
        .iter()
        .filter_map(|op| s.latency.get(op))
        .fold((0, 0), |(a, b), h| {
            (
                a.max(h.quantile_us(0.5).unwrap_or(0)),
                b.max(h.quantile_us(0.99).unwrap_or(0)),
            )
        });
    println!(
        "| {path:<44} | {events:>7} | {calls:>7} | {:>10.4} | {p50:>8} | {p99:>8} | {:>12.4} | {:>6.1} |",
        calls as f64 / events as f64,
        calls as f64 * USD_PER_CALL / events as f64 * 1e6,
        started.elapsed().as_secs_f64(),
    );
}

fn main() {
    let opts = CallOptions::default();
    std::sync::LazyLock::force(&RUN_STARTED);
    println!(
        "KMS path accounting. EMULATED provider ({EMULATED_LATENCY:?} per call): counts are contract facts, latency and price are not measurements."
    );
    println!("Envelope: at most {MAX_CALLS} calls and {MAX_RUN:?} per real run.\n");
    println!(
        "| path | events | provider calls | calls/event | p50 us (upper bound) | p99 us (upper bound) | USD per 1M events | wall s |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    let clock = Arc::new(ManualClock::new());

    // Write: one new object each.
    {
        let (_kms, meter) = provider();
        let m = manager(&meter, &clock, Duration::from_secs(300));
        meter.reset();
        let t = Instant::now();
        for n in 0..1000 {
            m.create_object_key(None, &ctx(n), 1, &opts).unwrap();
        }
        report("write: new object (wrap)", 1000, &meter, t);
    }
    // Warm read, cold read, cache off.
    for (label, ttl, step) in [
        (
            "read: warm cache (ttl 300 s)",
            Duration::from_secs(300),
            Duration::ZERO,
        ),
        (
            "read: cold, one read per lifetime",
            Duration::from_secs(300),
            Duration::from_secs(301),
        ),
        (
            "read: cache off (ttl 1 ms, every read cold)",
            Duration::from_millis(1),
            Duration::from_millis(2),
        ),
    ] {
        let (_kms, meter) = provider();
        let m = manager(&meter, &clock, ttl);
        let c = ctx(1);
        let (_d, wrapped) = m.create_object_key(None, &c, 1, &opts).unwrap();
        meter.reset();
        let t = Instant::now();
        let events = if step.is_zero() { 20_000 } else { 200 };
        for _ in 0..events {
            clock.advance(step);
            m.open_object_key(&wrapped, &c, 1, &opts).unwrap();
        }
        report(label, events, &meter, t);
    }
    // Revoked key: every refused attempt reaches the provider.
    {
        let (kms, meter) = provider();
        let m = manager(&meter, &clock, Duration::from_secs(300));
        let c = ctx(1);
        let (_d, wrapped) = m.create_object_key(None, &c, 1, &opts).unwrap();
        m.invalidate_index(&IndexId::new("orders").unwrap(), 1);
        kms.revoke(&key());
        meter.reset();
        let t = Instant::now();
        for _ in 0..200 {
            let _ = m.open_object_key(&wrapped, &c, 1, &opts);
        }
        report(
            "read: revoked key, caller does not consult the gate",
            200,
            &meter,
            t,
        );
    }
    println!(
        "\nRead the table with the evidence file: docs/plan/evidence/m05-08-kms-path-accounting.md"
    );
}
