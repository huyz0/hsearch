//! Request and contention measurements against an S3-compatible store (M02.10).
//!
//! `cargo run --release -p hsearch-s3 --example bench` prints one table per question:
//! how PUT latency and throughput grow with payload size (batching), how aggregate
//! PUT throughput scales with concurrent writers to distinct keys, and what a
//! contended compare-and-set costs when several writers race on one key.
//! Every number is provisional: one local store, loopback, no TLS, a shared host.
//!
//! Environment: `HSEARCH_S3_ENDPOINT`, `HSEARCH_S3_ACCESS_KEY`, `HSEARCH_S3_SECRET_KEY`;
//! optional `BENCH_SECONDS` (default 3) per cell.
use hsearch_object::{
    Budget, Cancellation, Error, Metadata, MetadataStore, Mutation, ObjectBackend, ObjectKey,
    Payload, Precondition, RequestContext, RequestId,
};
use hsearch_s3::{S3Config, S3Store};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

struct Thread(std::thread::Thread);
impl Wake for Thread {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(Thread(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::park();
    }
}

fn ctx() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(30),
        Cancellation::default(),
        Budget {
            request_bytes: 64 << 20,
            response_bytes: 1 << 20,
            attempts: 1_000_000,
        },
    )
    .unwrap()
}

fn mutation(id: &str, key: &str, body: &[u8], precondition: Precondition) -> Mutation {
    Mutation {
        request_id: RequestId::new(id.as_bytes()).unwrap(),
        key: ObjectKey::new(key.as_bytes()).unwrap(),
        payload: Payload::new(body).unwrap(),
        metadata: Metadata::new(b"bench").unwrap(),
        precondition,
    }
}

fn store(bucket: &str) -> Arc<S3Store> {
    let var = |n: &str| std::env::var(n).unwrap_or_else(|_| panic!("{n} is not set"));
    let store = S3Store::new(S3Config {
        endpoint: var("HSEARCH_S3_ENDPOINT"),
        bucket: bucket.into(),
        access_key: var("HSEARCH_S3_ACCESS_KEY"),
        secret_key: var("HSEARCH_S3_SECRET_KEY"),
        region: "us-east-1".into(),
    })
    .unwrap();
    store.prepare_bucket(&ctx()).unwrap();
    Arc::new(store)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

/// Run `threads` workers of `work(thread, iteration) -> bool` for `seconds`;
/// return (operations, successes, sorted latencies in ms, elapsed seconds).
fn run(
    threads: usize,
    seconds: f64,
    work: impl Fn(usize, u64) -> bool + Send + Sync + 'static,
) -> (u64, u64, Vec<f64>, f64) {
    let work = Arc::new(work);
    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(threads + 1));
    let (ops, wins) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let (work, stop, barrier, ops, wins) = (
                Arc::clone(&work),
                Arc::clone(&stop),
                Arc::clone(&barrier),
                Arc::clone(&ops),
                Arc::clone(&wins),
            );
            std::thread::spawn(move || {
                let mut latencies = Vec::new();
                barrier.wait();
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    let won = work(t, i);
                    latencies.push(started.elapsed().as_secs_f64() * 1000.0);
                    ops.fetch_add(1, Ordering::Relaxed);
                    wins.fetch_add(u64::from(won), Ordering::Relaxed);
                    i += 1;
                }
                latencies
            })
        })
        .collect();
    barrier.wait();
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(seconds));
    stop.store(true, Ordering::Relaxed);
    let mut latencies: Vec<f64> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    let elapsed = started.elapsed().as_secs_f64();
    latencies.sort_by(f64::total_cmp);
    (
        ops.load(Ordering::Relaxed),
        wins.load(Ordering::Relaxed),
        latencies,
        elapsed,
    )
}

fn main() {
    let seconds: f64 = std::env::var("BENCH_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3.0_f64)
        .max(0.1);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let s = store(&format!("bench-{nonce}"));
    println!(
        "cpus: {}, seconds per cell: {seconds}\n",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );

    println!("## PUT by payload size, one writer, distinct keys (batching)\n");
    println!("| payload | PUT/s | MiB/s | p50 ms | p99 ms |\n|---|---|---|---|---|");
    for size in [
        1usize << 10,
        16 << 10,
        256 << 10,
        1 << 20,
        4 << 20,
        16 << 20,
    ] {
        let body = Arc::new(vec![7u8; size]);
        let s2 = Arc::clone(&s);
        let (_, ops, lat, elapsed) = run(1, seconds, move |_, i| {
            let m = mutation(
                &format!("sz{size}-{i}"),
                &format!("size{size}/{i}"),
                &body,
                Precondition::Absent,
            );
            block(s2.create(&ctx(), &m)).is_ok()
        });
        println!(
            "| {} KiB | {:.0} | {:.1} | {:.1} | {:.1} |",
            size >> 10,
            ops as f64 / elapsed,
            ops as f64 * size as f64 / elapsed / (1 << 20) as f64,
            percentile(&lat, 0.5),
            percentile(&lat, 0.99)
        );
    }

    println!("\n## Concurrent PUTs of 64 KiB to distinct keys\n");
    println!("| writers | PUT/s | p50 ms | p99 ms |\n|---|---|---|---|");
    for writers in [1usize, 2, 4, 8, 16, 32] {
        let body = Arc::new(vec![7u8; 64 << 10]);
        let s2 = Arc::clone(&s);
        let (_, ops, lat, elapsed) = run(writers, seconds, move |t, i| {
            let m = mutation(
                &format!("cw{writers}-{t}-{i}"),
                &format!("conc{writers}/{t}/{i}"),
                &body,
                Precondition::Absent,
            );
            block(s2.create(&ctx(), &m)).is_ok()
        });
        println!(
            "| {writers} | {:.0} | {:.1} | {:.1} |",
            ops as f64 / elapsed,
            percentile(&lat, 0.5),
            percentile(&lat, 0.99)
        );
    }

    println!("\n## Contended compare-and-set on one 1 KiB key\n");
    println!(
        "| writers | attempts/s | winners/s | conflicts | other errors | p50 ms | p99 ms |\n|---|---|---|---|---|---|---|"
    );
    for writers in [1usize, 2, 4, 8, 16] {
        let key = format!("cas{writers}");
        let first = block(s.create(
            &ctx(),
            &mutation(
                &format!("{key}-seed"),
                &key,
                &[0u8; 1024],
                Precondition::Absent,
            ),
        ))
        .unwrap();
        let head = Arc::new(std::sync::Mutex::new(first.object.version));
        let (conflicts, other) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (s2, head2, c2, o2) = (
            Arc::clone(&s),
            Arc::clone(&head),
            Arc::clone(&conflicts),
            Arc::clone(&other),
        );
        let key2 = key.clone();
        let (ops, wins, lat, elapsed) = run(writers, seconds, move |t, i| {
            // Each attempt names the head it last saw, as a writer without a lock would.
            let seen = head2.lock().unwrap().clone();
            let m = mutation(
                &format!("{key2}-{t}-{i}"),
                &key2,
                &[1u8; 1024],
                Precondition::Matches(seen),
            );
            match block(s2.compare_exchange(&ctx(), &m)) {
                Ok(receipt) => {
                    *head2.lock().unwrap() = receipt.object.version;
                    true
                }
                Err(Error::Conflict) => {
                    c2.fetch_add(1, Ordering::Relaxed);
                    false
                }
                Err(_) => {
                    o2.fetch_add(1, Ordering::Relaxed);
                    false
                }
            }
        });
        println!(
            "| {writers} | {:.0} | {:.0} | {} | {} | {:.1} | {:.1} |",
            ops as f64 / elapsed,
            wins as f64 / elapsed,
            conflicts.load(Ordering::Relaxed),
            other.load(Ordering::Relaxed),
            percentile(&lat, 0.5),
            percentile(&lat, 0.99)
        );
    }
    println!("\nrequests: {:?}", s.counts());
}
