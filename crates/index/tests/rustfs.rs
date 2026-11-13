//! The write path against a real S3-compatible store (RustFS in CI): request
//! counts, and the crash matrix, in which a child process is aborted at each
//! point and a fresh process rebuilds from the store with its local files gone.
//! Needs `HSEARCH_S3_ENDPOINT` and credentials (see `scripts/rustfs.sh`); without
//! them each test prints SKIPPED unless `HSEARCH_S3_REQUIRED=1`.
mod support;

use hsearch_index::{Hooks, Index, Point, Wait, open_published};
use hsearch_s3::{S3Config, S3Store};
use std::io::Write;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};
use support::*;

static NEXT: AtomicU32 = AtomicU32::new(0);

fn config(bucket: String) -> Option<S3Config> {
    let Ok(endpoint) = std::env::var("HSEARCH_S3_ENDPOINT") else {
        assert!(
            std::env::var("HSEARCH_S3_REQUIRED").as_deref() != Ok("1"),
            "HSEARCH_S3_REQUIRED=1 but HSEARCH_S3_ENDPOINT is unset"
        );
        eprintln!(
            "SKIPPED: HSEARCH_S3_ENDPOINT is not set; nothing was checked against a real store"
        );
        return None;
    };
    Some(S3Config {
        endpoint,
        bucket,
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
}

fn fresh_bucket() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("hsi-{nanos}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn context() -> hsearch_object::RequestContext {
    hsearch_object::RequestContext::new(
        Duration::from_secs(60),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 1 << 28,
            response_bytes: 1 << 28,
            attempts: 10_000,
        },
    )
    .unwrap()
}

fn store_for(config: S3Config, prepare: bool) -> Arc<S3Store> {
    let store = S3Store::new(config).unwrap();
    if prepare {
        store.prepare_bucket(&context()).expect("prepare bucket");
    }
    Arc::new(store)
}

#[test]
fn requests_per_bulk_refresh_publish_and_open_never_list() {
    let Some(config) = config(fresh_bucket()) else {
        return;
    };
    let store = store_for(config, true);
    let (one, two, cache) = (
        Scratch::new("count-a"),
        Scratch::new("count-b"),
        Scratch::new("count-c"),
    );
    let before = store.counts();
    let index = Index::create(Arc::clone(&store), "orders", schema(), &one.0).unwrap();
    let created = store.counts();
    // create: root PUT, then open reads the root and probes slot 0, and bumps the root.
    assert_eq!(created.put - before.put, 2, "root create + root bump");
    assert_eq!(created.list, 0);

    let ops: Vec<_> = (0..100).map(|i| put(&format!("d{i}"), "x", i)).collect();
    index.bulk(ops, Wait::Searchable).unwrap();
    let bulk = store.counts();
    assert_eq!(bulk.put - created.put, 1, "one PUT for a whole bulk");
    assert_eq!(bulk.get - created.get, 0);
    assert_eq!(bulk.head - created.head, 0);
    assert_eq!(bulk.list, 0);

    index.refresh().unwrap();
    assert_eq!(store.counts(), bulk, "a refresh makes no store request");

    let published = index.publish().unwrap();
    let after_publish = store.counts();
    assert_eq!(
        after_publish.put - bulk.put,
        published.files as u64 + 1,
        "one PUT per new segment file plus the root"
    );
    assert_eq!(after_publish.list, 0);
    let live = all(&index.snapshot().unwrap());
    index.close();

    // Restart: the root, one head and one read per slot, one head past the end.
    let reopened = Index::open(Arc::clone(&store), "orders", schema(), &two.0).unwrap();
    let opened = store.counts();
    assert_eq!(opened.list, 0, "no LIST on the startup path");
    assert_eq!(opened.get - after_publish.get, 1 + 1, "root + the one slot");
    assert_eq!(
        opened.head - after_publish.head,
        1 + 1 + 1,
        "root + slot + end of log"
    );
    assert_eq!(all(&reopened.snapshot().unwrap()), live);

    // A search-only reader opens the published segments without any LIST.
    let served = open_published(Arc::clone(&store), "orders", schema(), &cache.0).unwrap();
    assert_eq!(all(&served), live);
    assert_eq!(store.counts().list, 0);
}

/// Aborts the process at one point, the `after`th time it is reached.
struct Abort {
    point: Point,
    after: usize,
    seen: AtomicU32,
}
impl Hooks for Abort {
    fn at(&self, point: Point) {
        if point == self.point
            && self.seen.fetch_add(1, Ordering::SeqCst) as usize + 1 == self.after
        {
            if point == Point::AfterAck {
                // Let the caller thread print the acknowledgement it just got.
                std::thread::sleep(Duration::from_millis(400));
            }
            std::process::abort();
        }
    }
}

const BULKS: usize = 6;
const PER_BULK: usize = 5;

/// The child half: runs a workload against the shared bucket and dies.
#[test]
fn crash_child() {
    let (Ok(point), Ok(bucket), Ok(scratch)) = (
        std::env::var("HSEARCH_CRASH_POINT"),
        std::env::var("HSEARCH_CRASH_BUCKET"),
        std::env::var("HSEARCH_CRASH_SCRATCH"),
    ) else {
        return;
    };
    let config = config(bucket).expect("endpoint");
    let store = store_for(config, false);
    let point = match point.as_str() {
        "before-slot" => Point::BeforeSlot,
        "after-slot" => Point::AfterSlot,
        "after-ack" => Point::AfterAck,
        "before-root" => Point::BeforeRoot,
        other => panic!("unknown point {other}"),
    };
    let hooks = Arc::new(Abort {
        point,
        after: if point == Point::BeforeRoot { 1 } else { 3 },
        seen: AtomicU32::new(0),
    });
    let index = Index::open_with(
        store,
        "crash",
        schema(),
        std::path::Path::new(&scratch),
        hooks,
    )
    .expect("open");
    let mut out = std::io::stdout();
    for bulk in 0..BULKS {
        let ops: Vec<_> = (0..PER_BULK)
            .map(|i| put(&format!("b{bulk}-{i}"), "x", (bulk * PER_BULK + i) as i64))
            .collect();
        index.bulk(ops, Wait::Durable).expect("bulk");
        writeln!(out, "ACK {bulk}").unwrap();
        out.flush().unwrap();
        if point == Point::BeforeRoot && bulk == 1 {
            index.publish().expect("publish");
        }
    }
    // Reaching here means the abort never fired.
    writeln!(out, "SURVIVED").unwrap();
}

fn run_crash(point: &str) {
    let Some(config) = config(fresh_bucket()) else {
        return;
    };
    let store = store_for(config.clone(), true);
    let (child_scratch, recovery, cache) = (
        Scratch::new("crash-child"),
        Scratch::new("crash-recover"),
        Scratch::new("crash-cache"),
    );
    // The index exists before the child starts; the child only writes.
    Index::create(Arc::clone(&store), "crash", schema(), &child_scratch.0)
        .unwrap()
        .close();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["crash_child", "--exact", "--nocapture", "--test-threads=1"])
        .env("HSEARCH_CRASH_POINT", point)
        .env("HSEARCH_CRASH_BUCKET", &config.bucket)
        .env("HSEARCH_CRASH_SCRATCH", &child_scratch.0)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "the child must die at {point}: {stdout}"
    );
    assert!(
        !stdout.contains("SURVIVED"),
        "the abort never fired at {point}"
    );
    let acked: Vec<usize> = stdout
        .lines()
        .filter_map(|l| l[l.find("ACK ")? + 4..].trim().parse().ok())
        .collect();
    // Every local file is gone before recovery.
    drop(child_scratch);

    let index = Index::open(Arc::clone(&store), "crash", schema(), &recovery.0).unwrap();
    let (total, ids) = all(&index.snapshot().unwrap());
    // Atomicity: each bulk is wholly there or wholly absent.
    let mut present = vec![0usize; BULKS];
    for id in &ids {
        let bulk: usize = id[1..id.find('-').unwrap()].parse().unwrap();
        present[bulk] += 1;
    }
    assert!(
        present.iter().all(|&n| n == 0 || n == PER_BULK),
        "{present:?}"
    );
    assert_eq!(total as usize, ids.len());
    // Zero acknowledged writes are missing.
    for bulk in &acked {
        assert_eq!(
            present[*bulk], PER_BULK,
            "{point}: acked bulk {bulk} missing; acked {acked:?}"
        );
    }
    // Nothing is visible that was not at least submitted: only the in-flight
    // bulk may be present without an ack, and only once its slot was durable.
    let inflight = acked.len();
    for (bulk, &n) in present.iter().enumerate() {
        if !acked.contains(&bulk) && n > 0 {
            assert_eq!(
                bulk, inflight,
                "{point}: unacknowledged bulk {bulk} is visible"
            );
            assert!(
                point == "after-slot",
                "{point}: a write that never reached the store is visible"
            );
        }
    }
    match point {
        "before-slot" => assert_eq!(present[inflight], 0),
        "after-slot" => assert_eq!(present[inflight], PER_BULK, "durable slot lost"),
        // The ack was sent for the third bulk, so it is acked and present
        // above; nothing after it was ever submitted.
        "after-ack" => {
            assert_eq!(acked, vec![0, 1, 2]);
            assert_eq!(present[inflight], 0);
        }
        _ => {}
    }
    if point == "before-root" {
        // Orphan segment files exist but no root names them.
        assert!(open_published(Arc::clone(&store), "crash", schema(), &cache.0).is_err());
    }
    // The recovered writer is fully usable: write, publish, serve.
    index
        .bulk(vec![put("after", "y", 999)], Wait::Searchable)
        .unwrap();
    index.publish().unwrap();
    let live = all(&index.snapshot().unwrap());
    let served = open_published(store, "crash", schema(), &cache.0).unwrap();
    assert_eq!(all(&served), live);
    assert_eq!(live.0 as usize, total as usize + 1);
}

#[test]
fn crash_before_the_slot_write() {
    run_crash("before-slot");
}

#[test]
fn crash_after_the_slot_write_before_the_ack() {
    run_crash("after-slot");
}

#[test]
fn crash_after_the_ack_before_apply() {
    run_crash("after-ack");
}

#[test]
fn crash_in_the_middle_of_a_publish() {
    run_crash("before-root");
}
