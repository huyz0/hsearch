//! M03.05 measurements against a real S3-compatible store (RustFS). Markdown to stdout.
//!
//!     eval "$(scripts/rustfs.sh up)"
//!     cargo run --release -p hsearch-lane --example catalog_profile -- [names]
//!
//! Request counts are the S3 adapter's wire attempts. Provisional: one shared host, a container,
//! a loopback store, no network.
use std::sync::Arc;
use std::time::{Duration, Instant};

use hsearch_format::{Durability, Id, Layout};
use hsearch_lane::{
    Advance, Change, LaneId, advance_head, block_on, create_catalog, create_head, read_head,
};
use hsearch_s3::{S3Config, S3Store};

fn id(n: u64) -> Id {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&n.to_be_bytes());
    out[15] = 0xA5;
    out
}

fn pct(samples: &mut [Duration], p: f64) -> f64 {
    samples.sort_unstable();
    let at = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples.get(at).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

fn main() {
    let names: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let endpoint = std::env::var("HSEARCH_S3_ENDPOINT").expect("HSEARCH_S3_ENDPOINT");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let store = S3Store::new(S3Config {
        endpoint,
        bucket: format!("hsc-{nanos}"),
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
            attempts: 100_000,
        },
    )
    .expect("ctx");
    store.prepare_bucket(&ctx).expect("bucket");
    let store = Arc::new(store);
    let layout = Layout::new("bench").expect("layout");

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

    // Structural head: establish transitions, one PUT each, version carried (no read).
    let lane = LaneId::new([7; 16]);
    let (mut h, mut v) =
        block_on(create_head(&*store, &layout, &lane, Durability::Object)).expect("head");
    let (c0, mut lat) = (store.counts(), Vec::new());
    for fence in 1..=100u64 {
        let t = Instant::now();
        let Advance::Done(nh, nv) = block_on(advance_head(
            &*store,
            &layout,
            &lane,
            &h,
            &v,
            Change::Establish {
                activation: 1,
                lane: 0,
                fence,
            },
        ))
        .expect("advance") else {
            panic!("lost with no contender")
        };
        lat.push(t.elapsed());
        (h, v) = (nh, nv);
    }
    row("head transition (version held)", 100, c0, lat);
    let (c0, mut lat) = (store.counts(), Vec::new());
    for _ in 0..100 {
        let t = Instant::now();
        block_on(read_head(&*store, &layout, &lane)).expect("read");
        lat.push(t.elapsed());
    }
    row("head read (100 epochs in inventory)", 100, c0, lat);

    // Catalog: create names into a catalog sized so pages stay small, then resolve and enumerate.
    let bits = 8u8;
    let catalog = block_on(create_catalog(&*store, &layout, [3; 16], bits)).expect("catalog");
    let (c0, mut lat) = (store.counts(), Vec::new());
    for n in 0..names {
        let t = Instant::now();
        block_on(catalog.create(
            &*store,
            &layout,
            &format!("tenant-0001/index-{n:07}"),
            id(n + 1),
        ))
        .expect("create");
        lat.push(t.elapsed());
    }
    row(
        &format!("catalog create ({} buckets)", 1u32 << bits),
        names as usize,
        c0,
        lat,
    );
    let (c0, mut lat) = (store.counts(), Vec::new());
    for n in (0..names).step_by(7) {
        let t = Instant::now();
        block_on(catalog.resolve(&*store, &layout, &format!("tenant-0001/index-{n:07}")))
            .expect("resolve");
        lat.push(t.elapsed());
    }
    let sample = lat.len();
    row("catalog resolve", sample, c0, lat);
    let (c0, mut lat) = (store.counts(), Vec::new());
    let (mut cursor, mut total, mut calls) = (None, 0usize, 0usize);
    loop {
        let t = Instant::now();
        let (page, next) =
            block_on(catalog.enumerate(&*store, &layout, cursor, 100)).expect("enumerate");
        lat.push(t.elapsed());
        total += page.len();
        calls += 1;
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(total as u64, names, "enumeration returned every name");
    row("catalog enumerate (100 per call)", calls, c0, lat);
    // Contention: four threads create distinct names into a two-page catalog, so most creates race
    // on a page. A lost race is resolved by the adapter, which lists versions.
    let hot = block_on(create_catalog(&*store, &layout, [4; 16], 1)).expect("hot catalog");
    let c0 = store.counts();
    let started = Instant::now();
    let per_thread = 100u64;
    let handles: Vec<_> = (0..4u64)
        .map(|t| {
            let (store, hot, layout) = (Arc::clone(&store), hot.clone(), layout.clone());
            std::thread::spawn(move || {
                let mut lat = Vec::new();
                for n in 0..per_thread {
                    let at = Instant::now();
                    block_on(hot.create(
                        &*store,
                        &layout,
                        &format!("t{t}-{n}"),
                        id(1_000_000 * (t + 1) + n),
                    ))
                    .expect("contended create");
                    lat.push(at.elapsed());
                }
                lat
            })
        })
        .collect();
    let lat: Vec<Duration> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("thread"))
        .collect();
    let elapsed = started.elapsed();
    row("catalog create, 4 writers on 2 pages", lat.len(), c0, lat);
    println!("\ncontended run took {:.2} s", elapsed.as_secs_f64());
    let t = Instant::now();
    let report = block_on(catalog.verify(&*store, &layout)).expect("verify");
    println!(
        "\nverify: {} pages, {} bad, {} live in {:.2} s",
        report.pages,
        report.pages_bad,
        report.live,
        t.elapsed().as_secs_f64()
    );
}
