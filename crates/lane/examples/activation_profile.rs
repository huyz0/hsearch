//! M13.01 measurements. Markdown to stdout.
//!
//!     cargo run --release -p hsearch-lane --example activation_profile -- footprint <idle> set|registry
//!     eval "$(scripts/rustfs.sh up)"
//!     cargo run --release -p hsearch-lane --example activation_profile -- cold [tenants] [indexes_per_tenant]
//!
//! `footprint` is a model: it compares what a process holds when idle indexes have no entry (the
//! active set) against one record per index (the registry a polling or per-index-timer design
//! would keep). `cold` runs against RustFS: the requests a cold activation costs, the cost of idle
//! indexes, and a burst of cold activations through the capped set. Provisional: one shared host,
//! a container, a loopback store, no network.
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hsearch_format::{Durability, Id, Layout};
use hsearch_lane::activation::{ActiveSet, Begin, Limits, Refusal};
use hsearch_lane::partition::bucket_bits_for;
use hsearch_lane::{LaneId, block_on, create_catalog, create_head, read_head};
use hsearch_s3::{S3Config, S3Store};

fn id(n: u64) -> Id {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&n.to_be_bytes());
    out[15] = 0xA5;
    out
}

fn rss() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

fn footprint(idle: u64, kind: &str) {
    let before = rss();
    let held;
    let mut detail = String::new();
    match kind {
        "registry" => {
            // One record per index: its id, a state word and a next-check time.
            let mut all: HashMap<Id, [u64; 4]> = HashMap::with_capacity(idle as usize);
            for n in 0..idle {
                all.insert(id(n), [n, 0, 0, 0]);
            }
            held = all.len();
            std::hint::black_box(&all);
            println!(
                "| registry | {idle} | {} | {} |{detail}",
                (rss() - before) / (1 << 20),
                held
            );
        }
        _ => {
            let mut set: ActiveSet<[u64; 4]> = ActiveSet::new(Limits {
                max_active: 4096,
                max_bytes: 1 << 40,
                max_loading: 64,
            })
            .expect("limits");
            let mut x = 0x9E37_79B9_7F4A_7C15u64;
            let mut worst = 0;
            for _ in 0..200_000u64 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let n = x % idle.max(1);
                let s0 = set.stats().steps;
                match set.begin(id(n), 1 << 20).expect("begin") {
                    Begin::Load { ticket, .. } => {
                        set.finish(ticket, [n, 0, 0, 0], 1 << 20).expect("finish");
                    }
                    Begin::Hit | Begin::Pending => {}
                }
                worst = worst.max(set.stats().steps - s0);
            }
            held = set.len();
            detail = format!(
                " set {} KiB, worst {worst} steps/op",
                set.footprint_bytes() / 1024
            );
            std::hint::black_box(&set);
            println!(
                "| set (cap 4096) | {idle} | {} | {} |{detail}",
                (rss() - before) / (1 << 20),
                held
            );
        }
    }
}

fn pct(samples: &mut [Duration], p: f64) -> f64 {
    samples.sort_unstable();
    let at = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples.get(at).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

fn cold(tenants: u64, per_tenant: u64) {
    let endpoint = std::env::var("HSEARCH_S3_ENDPOINT").expect("HSEARCH_S3_ENDPOINT");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let store = S3Store::new(S3Config {
        endpoint,
        bucket: format!("hsa-{nanos}"),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
    .expect("store");
    let ctx = hsearch_object::RequestContext::new(
        Duration::from_secs(60),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 1 << 28,
            response_bytes: 1 << 28,
            attempts: 1_000_000,
        },
    )
    .expect("ctx");
    store.prepare_bucket(&ctx).expect("bucket");
    let layout = Layout::new("act").expect("layout");

    // One catalog per tenant, sized by the strategy; one shared lane head for the cohort.
    let bits = bucket_bits_for(per_tenant);
    let lane = LaneId::new([9; 16]);
    block_on(create_head(&store, &layout, &lane, Durability::Object)).expect("head");
    let t0 = Instant::now();
    let mut catalogs = Vec::new();
    for t in 0..tenants {
        let catalog =
            block_on(create_catalog(&store, &layout, id(1_000_000 + t), bits)).expect("catalog");
        for i in 0..per_tenant {
            block_on(catalog.create(
                &store,
                &layout,
                &format!("index-{i:04}"),
                id(t * per_tenant + i + 1),
            ))
            .expect("create");
        }
        catalogs.push(catalog);
    }
    let c = store.counts();
    println!(
        "Setup: {tenants} tenants x {per_tenant} names = {} indexes, {bits} bucket bits, {} PUT, {:.1} s.\n",
        tenants * per_tenant,
        c.put,
        t0.elapsed().as_secs_f64()
    );

    // 1. Idle cost: nothing runs for an idle index.
    let c0 = store.counts();
    std::thread::sleep(Duration::from_secs(3));
    let c1 = store.counts();
    println!(
        "Idle: {} requests in 3 s with {} indexes catalogued and none active.\n",
        (c1.put + c1.get + c1.head + c1.list) - (c0.put + c0.get + c0.head + c0.list),
        tenants * per_tenant
    );

    // 2. One cold activation: resolve the name, read the lane head. A second one on the same lane reuses the head.
    let mut set: ActiveSet<Id> = ActiveSet::new(Limits {
        max_active: 64,
        max_bytes: 1 << 30,
        max_loading: 8,
    })
    .expect("limits");
    let activate = |set: &mut ActiveSet<Id>, t: u64, i: u64, with_head: bool| -> Duration {
        let started = Instant::now();
        let catalog = &catalogs[t as usize];
        let index = match block_on(catalog.resolve(&store, &layout, &format!("index-{i:04}")))
            .expect("resolve")
        {
            hsearch_lane::Resolution::Live { index, .. } => index,
            other => panic!("{other:?}"),
        };
        if with_head {
            block_on(read_head(&store, &layout, &lane)).expect("head");
        }
        match set.begin(index, 1 << 20).expect("begin") {
            Begin::Load { ticket, .. } => {
                set.finish(ticket, index, 1 << 20).expect("finish");
            }
            other => panic!("{other:?}"),
        }
        started.elapsed()
    };
    println!(
        "| step | n | PUT | GET | HEAD | LIST | p50 ms | p99 ms |\n|---|---|---|---|---|---|---|---|"
    );
    let row = |step: &str, n: usize, c0: hsearch_s3::RequestCounts, mut s: Vec<Duration>| {
        let c = store.counts();
        println!(
            "| {step} | {n} | {:.2} | {:.2} | {:.2} | {} | {:.1} | {:.1} |",
            (c.put - c0.put) as f64 / n as f64,
            (c.get - c0.get) as f64 / n as f64,
            (c.head - c0.head) as f64 / n as f64,
            c.list - c0.list,
            pct(&mut s, 0.5),
            pct(&mut s, 0.99)
        );
    };
    let n = 40u64.min(tenants * per_tenant) as usize;
    let c0 = store.counts();
    let lat: Vec<_> = (0..n as u64)
        .map(|k| activate(&mut set, k % tenants, k / tenants % per_tenant, true))
        .collect();
    row("cold activation (resolve + lane head)", n, c0, lat);
    // A hit costs nothing.
    let c0 = store.counts();
    let mut lat = Vec::new();
    for k in 0..n as u64 {
        let t = Instant::now();
        let index = id((k % tenants) * per_tenant + (k / tenants % per_tenant) + 1);
        assert!(matches!(
            set.begin(index, 1 << 20).expect("begin"),
            Begin::Hit
        ));
        lat.push(t.elapsed());
    }
    row("active (hit)", n, c0, lat);

    // 3. A burst: every index cold at once, 8 workers, 64 active at most, 8 loads at most.
    let total = tenants * per_tenant;
    let burst = ActiveSet::<Id>::new(Limits {
        max_active: 64,
        max_bytes: 1 << 30,
        max_loading: 8,
    })
    .expect("limits");
    let shared = Mutex::new(burst);
    let next = AtomicU64::new(0);
    let retries = AtomicU64::new(0);
    let peak = AtomicU64::new(0);
    let c0 = store.counts();
    let started = Instant::now();
    let lat = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..16 {
            scope.spawn(|| {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= total {
                        break;
                    }
                    let (t, i) = (k % tenants, k / tenants % per_tenant);
                    let began = Instant::now();
                    let catalog = &catalogs[t as usize];
                    loop {
                        let index = id(t * per_tenant + i + 1);
                        let step = shared.lock().expect("lock").begin(index, 1 << 20);
                        match step {
                            Ok(Begin::Load { ticket, .. }) => {
                                let ok = block_on(catalog.resolve(
                                    &store,
                                    &layout,
                                    &format!("index-{i:04}"),
                                ))
                                .is_ok()
                                    && block_on(read_head(&store, &layout, &lane)).is_ok();
                                let mut guard = shared.lock().expect("lock");
                                if ok {
                                    guard.finish(ticket, index, 1 << 20).expect("finish");
                                } else {
                                    guard.fail(ticket);
                                }
                                peak.fetch_max(guard.len() as u64, Ordering::Relaxed);
                                break;
                            }
                            Ok(Begin::Hit) => break,
                            Ok(Begin::Pending) | Err(Refusal::Loading) => {
                                retries.fetch_add(1, Ordering::Relaxed);
                                std::thread::yield_now();
                            }
                            Err(other) => panic!("{other:?}"),
                        }
                    }
                    lat.lock().expect("lock").push(began.elapsed());
                }
            });
        }
    });
    let elapsed = started.elapsed();
    let stats = shared.lock().expect("lock").stats();
    let mut lat = lat.into_inner().expect("lock");
    println!(
        "\nBurst: {total} cold indexes, 16 workers, caps 64 active / 8 loading. {:.1} s ({:.0} activations/s), peak active {}, {} evictions, {} loads, {} waits on the loading cap, p50 {:.1} ms, p99 {:.1} ms; {:.2} GET + {:.2} HEAD per activation, {} PUT, {} LIST.",
        elapsed.as_secs_f64(),
        total as f64 / elapsed.as_secs_f64(),
        peak.load(Ordering::Relaxed),
        stats.evictions,
        stats.activations,
        retries.load(Ordering::Relaxed),
        pct(&mut lat, 0.5),
        pct(&mut lat, 0.99),
        (store.counts().get - c0.get) as f64 / total as f64,
        (store.counts().head - c0.head) as f64 / total as f64,
        store.counts().put - c0.put,
        store.counts().list - c0.list,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("footprint") => footprint(
            args.get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(1_000_000),
            args.get(2).map_or("set", String::as_str),
        ),
        Some("cold") => cold(
            args.get(1).and_then(|s| s.parse().ok()).unwrap_or(40),
            args.get(2).and_then(|s| s.parse().ok()).unwrap_or(25),
        ),
        _ => eprintln!("usage: footprint <idle> set|registry | cold [tenants] [per_tenant]"),
    }
}
