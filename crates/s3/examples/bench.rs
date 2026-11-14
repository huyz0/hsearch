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

/// A body of `len` bytes that differs for every (writer, iteration). `If-Match` compares the
/// ETag, a content hash: identical bodies share an ETag, so a stale head would still match and
/// the compare-and-set would fence nothing. A contention number from identical bodies is wrong.
fn unique_body(len: usize, t: usize, i: u64) -> Vec<u8> {
    let mut body = vec![7u8; len];
    body[..8].copy_from_slice(&(t as u64).to_le_bytes());
    body[8..16].copy_from_slice(&i.to_le_bytes());
    body
}

/// Distinct error variants seen by the workers, printed once at the end so "other errors" is
/// not an unexplained number.
static ERRORS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn note_error(error: &Error) {
    let name = format!("{error:?}");
    let mut seen = ERRORS.lock().unwrap();
    if !seen.contains(&name) && seen.len() < 16 {
        seen.push(name);
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
    pause: Duration,
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
                    if !pause.is_zero() {
                        // Between operations, outside the timed section: open-loop-ish pacing.
                        std::thread::sleep(pause);
                    }
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

    // BENCH_ONLY=open_loop (or control, matched) runs just that section.
    let only = std::env::var("BENCH_ONLY").unwrap_or_default();
    let want = |name: &str| only.is_empty() || only == name;
    if want("tables") {
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
            let (_, ops, lat, elapsed) = run(1, seconds, Duration::ZERO, move |_, i| {
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
            let (_, ops, lat, elapsed) = run(writers, seconds, Duration::ZERO, move |t, i| {
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
            let (ops, wins, lat, elapsed) = run(writers, seconds, Duration::ZERO, move |t, i| {
                // Each attempt names the head it last saw, as a writer without a lock would.
                let seen = head2.lock().unwrap().clone();
                let m = mutation(
                    &format!("{key2}-{t}-{i}"),
                    &key2,
                    &unique_body(1024, t, i),
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
                    Err(error) => {
                        note_error(&error);
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
    }
    if want("matched") {
        matched(&s, seconds, &nonce.to_string());
    }
    if want("control") {
        control(&s, seconds);
    }
    if want("open_loop") {
        open_loop(&s, seconds);
    }
    println!("\nrequests: {:?}", s.counts());
    println!("other error variants seen: {:?}", ERRORS.lock().unwrap());
}

/// Matched sparse/busy/skewed workloads: same payload (16 KiB), same duration, same client and
/// store, same measurement. Only the arrival shape differs:
/// - sparse: one writer, one commit attempt every 50 ms, its own key;
/// - busy: 16 closed-loop writers, each on its own key (no contention);
/// - skewed: 16 closed-loop writers, 90% of attempts on one hot key (shared head, real
///   contention), 10% on the writer's own key.
///
/// Requests per commit are the store's own request counters, per attempt including failures.
fn matched(s: &Arc<S3Store>, seconds: f64, nonce: &str) {
    println!("\n## Matched workloads, 16 KiB commits, compare-and-set on a per-key head\n");
    println!(
        "| shape | writers | attempts/s | commits/s | conflicts | other errors | p50 ms | p99 ms | PUT/commit | HEAD/commit | GET/commit | LIST/commit |\n|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    for (shape, writers, pause_ms, hot_pct) in [
        ("sparse", 1usize, 50u64, 0u64),
        ("busy", 16, 0, 0),
        ("skewed", 16, 0, 90),
    ] {
        let body = vec![3u8; 16 << 10];
        let hot = format!("matched-{shape}-{nonce}-hot");
        let first = block(s.create(
            &ctx(),
            &mutation(&format!("{hot}-seed"), &hot, &body, Precondition::Absent),
        ))
        .unwrap();
        let hot_head = Arc::new(std::sync::Mutex::new(first.object.version));
        let own: Arc<Vec<std::sync::Mutex<Option<hsearch_object::VersionToken>>>> =
            Arc::new((0..writers).map(|_| std::sync::Mutex::new(None)).collect());
        let (conflicts, other) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let before = s.counts();
        let (s2, hot2, own2, c2, o2) = (
            Arc::clone(s),
            Arc::clone(&hot_head),
            Arc::clone(&own),
            Arc::clone(&conflicts),
            Arc::clone(&other),
        );
        let (shape_s, hot_key) = (shape.to_string(), hot.clone());
        let (_, wins, lat, elapsed) = run(
            writers,
            seconds,
            Duration::from_millis(pause_ms),
            move |t, i| {
                let on_hot = hot_pct > 0 && (i * 7 + t as u64 * 13) % 100 < hot_pct;
                let key = if on_hot {
                    hot_key.clone()
                } else {
                    format!("matched-{shape_s}-{t}-{}", hot_key.len())
                };
                let seen = if on_hot {
                    Some(hot2.lock().unwrap().clone())
                } else {
                    own2[t].lock().unwrap().clone()
                };
                let precondition = match &seen {
                    Some(v) => Precondition::Matches(v.clone()),
                    None => Precondition::Absent,
                };
                let m = mutation(
                    &format!("{key}-{t}-{i}"),
                    &key,
                    &unique_body(16 << 10, t, i),
                    precondition,
                );
                let result = if seen.is_some() {
                    block(s2.compare_exchange(&ctx(), &m))
                } else {
                    block(s2.create(&ctx(), &m))
                };
                match result {
                    Ok(receipt) => {
                        if on_hot {
                            *hot2.lock().unwrap() = receipt.object.version;
                        } else {
                            *own2[t].lock().unwrap() = Some(receipt.object.version);
                        }
                        true
                    }
                    Err(Error::Conflict) => {
                        c2.fetch_add(1, Ordering::Relaxed);
                        false
                    }
                    Err(error) => {
                        note_error(&error);
                        o2.fetch_add(1, Ordering::Relaxed);
                        false
                    }
                }
            },
        );
        let after = s.counts();
        let per = |a: u64, b: u64| (a - b) as f64 / wins.max(1) as f64;
        println!(
            "| {shape} | {writers} | {:.0} | {:.0} | {} | {} | {:.1} | {:.1} | {:.2} | {:.2} | {:.2} | {:.2} |",
            lat.len() as f64 / elapsed,
            wins as f64 / elapsed,
            conflicts.load(Ordering::Relaxed),
            other.load(Ordering::Relaxed),
            percentile(&lat, 0.5),
            percentile(&lat, 0.99),
            per(after.put, before.put),
            per(after.head, before.head),
            per(after.get, before.get),
            per(after.list, before.list),
        );
    }
}

/// Deliberately failing control for the contended compare-and-set table: the same loop, but every
/// attempt writes the SAME body. If `If-Match` is an ETag (content hash) comparison, a stale head
/// still matches, so conflicts collapse toward zero and the CAS fences nothing. The expected
/// outcome of this control is therefore "no conflicts at 16 writers", which would be wrong for a
/// fencing primitive; the distinct-body table above is the real measurement.
fn control(s: &Arc<S3Store>, seconds: f64) {
    println!("\n## Control (expected to expose missing fencing): identical bodies, one hot key\n");
    let key = "control-identical";
    let body = vec![9u8; 1024];
    let first = block(s.create(
        &ctx(),
        &mutation("control-seed", key, &body, Precondition::Absent),
    ))
    .unwrap();
    // Direct probe: update from v1 twice with identical bytes; a fenced CAS refuses the second.
    let v1 = first.object.version.clone();
    let a = block(s.compare_exchange(
        &ctx(),
        &mutation("probe-a", key, &body, Precondition::Matches(v1.clone())),
    ));
    let b = block(s.compare_exchange(
        &ctx(),
        &mutation("probe-b", key, &body, Precondition::Matches(v1)),
    ));
    println!(
        "probe: first update from v1 with identical bytes: {}; second update from the same stale v1: {}\n",
        if a.is_ok() { "accepted" } else { "refused" },
        match &b {
            Ok(_) => "ACCEPTED (stale head not fenced)".to_string(),
            Err(Error::Conflict) => "refused with Conflict (fenced)".to_string(),
            Err(e) => format!("error {e:?}"),
        }
    );
    println!(
        "| writers | attempts/s | winners/s | conflicts | other errors |\n|---|---|---|---|---|"
    );
    let head = Arc::new(std::sync::Mutex::new(
        a.map(|r| r.object.version)
            .unwrap_or_else(|_| first.object.version.clone()),
    ));
    let (conflicts, other) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (s2, h2, c2, o2, body2) = (
        Arc::clone(s),
        Arc::clone(&head),
        Arc::clone(&conflicts),
        Arc::clone(&other),
        body.clone(),
    );
    let (ops, wins, _, elapsed) = run(16, seconds, Duration::ZERO, move |t, i| {
        let seen = h2.lock().unwrap().clone();
        let m = mutation(
            &format!("control-{t}-{i}"),
            key,
            &body2,
            Precondition::Matches(seen),
        );
        match block(s2.compare_exchange(&ctx(), &m)) {
            Ok(r) => {
                *h2.lock().unwrap() = r.object.version;
                true
            }
            Err(Error::Conflict) => {
                c2.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(e) => {
                note_error(&e);
                o2.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    });
    println!(
        "| 16 | {:.0} | {:.0} | {} | {} |",
        ops as f64 / elapsed,
        wins as f64 / elapsed,
        conflicts.load(Ordering::Relaxed),
        other.load(Ordering::Relaxed)
    );
}

/// Open-loop batching sweep: PUTs arrive at a fixed rate whether or not earlier ones finished, on
/// distinct keys, served by up to 32 workers. Latency is measured from the scheduled arrival, so
/// queueing past saturation shows up (a closed loop hides it). `dropped` counts arrivals still
/// unstarted when the cell ended.
fn open_loop(s: &Arc<S3Store>, seconds: f64) {
    println!("\n## Open-loop PUT sweep (fixed arrival rate, distinct keys, 32 workers)\n");
    println!(
        "| payload | offered/s | completed/s | errors | p50 ms | p99 ms | unstarted | MiB/s |\n|---|---|---|---|---|---|---|---|"
    );
    for (size, rates) in [
        (64usize << 10, vec![200.0_f64, 500.0, 800.0, 1200.0]),
        (1 << 20, vec![30.0, 60.0, 90.0, 120.0]),
        (4 << 20, vec![10.0, 20.0, 30.0]),
    ] {
        for rate in rates {
            let body = Arc::new(vec![5u8; size]);
            let (tx, rx) = std::sync::mpsc::channel::<(u64, Instant)>();
            let rx = Arc::new(std::sync::Mutex::new(rx));
            let lats = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let start = Instant::now();
            let started = Arc::new(AtomicU64::new(0));
            let failed = Arc::new(AtomicU64::new(0));
            let last_done = Arc::new(std::sync::Mutex::new(Duration::ZERO));
            let workers: Vec<_> = (0..32)
                .map(|_| {
                    let (s2, rx2, l2, b2, st, started2, failed2, last2) = (
                        Arc::clone(s),
                        Arc::clone(&rx),
                        Arc::clone(&lats),
                        Arc::clone(&body),
                        Arc::clone(&stop),
                        Arc::clone(&started),
                        Arc::clone(&failed),
                        Arc::clone(&last_done),
                    );
                    std::thread::spawn(move || {
                        loop {
                            let job = rx2.lock().unwrap().recv_timeout(Duration::from_millis(50));
                            match job {
                                Ok((i, at)) => {
                                    started2.fetch_add(1, Ordering::Relaxed);
                                    let m = mutation(
                                        &format!("ol{size}-{rate}-{i}"),
                                        &format!("openloop{size}/{rate}/{i}"),
                                        &b2,
                                        Precondition::Absent,
                                    );
                                    if block(s2.create(&ctx(), &m)).is_ok() {
                                        l2.lock().unwrap().push(at.elapsed().as_secs_f64() * 1e3);
                                        *last2.lock().unwrap() = start.elapsed();
                                    } else {
                                        failed2.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Err(_) if st.load(Ordering::Relaxed) => break,
                                Err(_) => {}
                            }
                        }
                    })
                })
                .collect();
            let mut sent = 0u64;
            while start.elapsed().as_secs_f64() < seconds {
                let due = Duration::from_secs_f64(sent as f64 / rate);
                if let Some(wait) = due.checked_sub(start.elapsed()) {
                    std::thread::sleep(wait);
                }
                let at = start + due;
                tx.send((sent, at)).unwrap();
                sent += 1;
            }
            // Give queued work one more cell-length to drain, then count what never started.
            let drain = Instant::now();
            while drain.elapsed().as_secs_f64() < seconds
                && (lats.lock().unwrap().len() as u64) + failed.load(Ordering::Relaxed) < sent
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            stop.store(true, Ordering::Relaxed);
            for w in workers {
                w.join().unwrap();
            }
            let dropped = sent - started.load(Ordering::Relaxed);
            let mut lat = lats.lock().unwrap().clone();
            lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let done = lat.len() as f64;
            // Completions over the time until the last one finished: below saturation this is
            // the offered window, past it the longer drain, so it shows the real completion rate.
            let span = last_done.lock().unwrap().as_secs_f64().max(seconds);
            println!(
                "| {} KiB | {rate:.0} | {:.0} | {} | {:.1} | {:.1} | {dropped} | {:.1} |",
                size >> 10,
                done / span,
                failed.load(Ordering::Relaxed),
                percentile(&lat, 0.5),
                percentile(&lat, 0.99),
                done * size as f64 / span / (1 << 20) as f64
            );
        }
    }
}
