//! Hosts of several indexes on one lane through one lane thread: each serves exactly what it
//! acknowledged across restarts, records of different indexes share a slot, and a lost fence
//! breaks every host. In-memory store, so timings are provisional.
#[path = "support/history.rs"]
mod history;
mod support;

use std::sync::{Arc, Barrier};
use std::time::Duration;

use history::*;
use hsearch_commit::Dependencies;
use hsearch_engine::StoredValue;
use hsearch_index::{Field, Op, Wait};
use hsearch_lane::LaneId;
use hsearch_store::host::{Config, Host, HostError, LogArgs};
use hsearch_store::log::Log;
use hsearch_store::publish::AnyDependencies;
use hsearch_store::shared::SharedLane;
use support::*;

fn deps() -> Dependencies {
    Dependencies {
        schema: [1; 32],
        crypto: [1; 32],
        resolved: [1; 32],
    }
}

fn put(id: &str, tag: &str) -> Op {
    Op::Put {
        id: id.into(),
        fields: vec![Field::Str(Arc::new(tag.into()))],
    }
}

fn config(label: &str, index: hsearch_format::Id, base_every: u64, seed: u8) -> Config {
    Config {
        index,
        shard: SHARD,
        incarnation: 1,
        scratch: std::env::temp_dir()
            .join(format!("hsearch-shared-{label}-{}", std::process::id())),
        limits: big(),
        deps: deps(),
        engine_format: "lucene-test".into(),
        mapping: [5; 32],
        crypto_envelope: [6; 16],
        window: Duration::ZERO,
        base_every_batches: base_every,
        token_seed: [seed; 8],
        merge: hsearch_engine::MergeMode::Inline,
        searchable_when_empty: false,
        base: hsearch_store::pointer::BaseSource::Pointer,
        pin: None,
    }
}

fn open(store: &Arc<Crash>, lane: LaneId, shared: &SharedLane, config: Config) -> Host {
    Host::open_log(
        LogArgs {
            store: Arc::clone(store),
            publisher: publisher(store),
            log: shared.handle(),
            lane,
            dependencies: AnyDependencies,
            schema: schema(),
            payloads: None,
        },
        config,
    )
    .unwrap()
}

fn tag(host: &Host, id: &str) -> Option<String> {
    let reader = host.reader()?;
    reader.reader.get(id, &["tag"]).unwrap().map(|hit| {
        let StoredValue::String(tag) = &hit.fields[0].1 else {
            panic!("tag is a string")
        };
        tag.clone()
    })
}

fn lane_thread(store: &Arc<Crash>, lane: LaneId, wait_ms: u64) -> SharedLane {
    let (w, _) = hsearch_lane::block_on(hsearch_lane::LaneWriter::open_with_head(
        Arc::clone(store),
        layout(),
        lane,
        |_| Ok(()),
    ))
    .unwrap();
    // Room for every host's record in one slot.
    let policy = hsearch_lane::batch::BatchPolicy {
        max_records: 64,
        max_bytes: 4 << 20,
        max_wait: Duration::from_millis(1),
    };
    let committer =
        hsearch_commit::Committer::new(w, hsearch_commit::AnyDependencies, policy).unwrap();
    SharedLane::start(committer, Duration::from_millis(wait_ms), 64)
}

#[test]
fn hosts_of_two_indexes_write_one_lane_and_each_restart_serves_exactly_its_acknowledged_writes() {
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    let mut expected: [Vec<String>; 2] = [Vec::new(), Vec::new()];
    for round in 0..3u8 {
        // Each round is a restart of both hosts over a fresh lane thread (which fences the last).
        let shared = lane_thread(&store, lane, 0);
        let a = open(
            &store,
            lane,
            &shared,
            config(&format!("a{round}"), INDEX, 2, round * 2 + 1),
        );
        let b = open(
            &store,
            lane,
            &shared,
            config(&format!("b{round}"), B, 2, round * 2 + 2),
        );
        for (host, name, list) in [(&a, "a", 0), (&b, "b", 1)] {
            for i in 0..3 {
                let id = format!("{name}{round}-{i}");
                host.bulk(vec![put(&id, name)], Wait::Durable).unwrap();
                expected[list].push(id);
            }
        }
        for (host, name, list) in [(&a, "a", 0), (&b, "b", 1)] {
            host.refresh().unwrap();
            for id in &expected[list] {
                assert_eq!(tag(host, id).as_deref(), Some(name), "{id}");
            }
            // Nothing of the other index leaked in.
            for id in &expected[1 - list] {
                assert_eq!(tag(host, id), None, "{id}");
            }
        }
        a.stop();
        b.stop();
    }
}

#[test]
fn records_of_different_indexes_share_one_lane_slot() {
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    let shared = lane_thread(&store, lane, 500);
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = [(INDEX, 0u64), (B, 0u64)]
        .into_iter()
        .map(|(index, seq)| {
            let mut handle = shared.handle();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                handle.commit(vec![req(index, seq, "x")])
            })
        })
        .collect();
    let slots: Vec<u64> = threads
        .into_iter()
        .map(|t| match t.join().unwrap().remove(0) {
            hsearch_commit::Outcome::Committed(r) => r.ordinal,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(slots[0], slots[1], "one slot carries both indexes");
}

#[test]
fn a_fence_lost_by_the_lane_breaks_every_host() {
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    let shared = lane_thread(&store, lane, 0);
    let a = open(&store, lane, &shared, config("fa", INDEX, 0, 1));
    let b = open(&store, lane, &shared, config("fb", B, 0, 2));
    a.bulk(vec![put("x", "a")], Wait::Durable).unwrap();
    // Another writer takes the lane: the shared one is fenced.
    let _taker = writer(&store, lane);
    let lost = a.bulk(vec![put("y", "a")], Wait::Durable);
    assert!(lost.is_err(), "{lost:?}");
    // The other host never wrote after that, yet it is broken too.
    let other = b.bulk(vec![put("z", "b")], Wait::Durable);
    assert!(
        matches!(
            other,
            Err(HostError::Broken | HostError::Rejected(hsearch_commit::Reject::Poisoned))
        ),
        "{other:?}"
    );
    assert!(shared.handle().is_poisoned());
}

#[test]
fn the_shared_path_costs_a_bounded_multiple_of_the_direct_committer() {
    // Provisional: relative, in-memory store. Sequential single-record commits, the worst case for
    // a hand-off, are timed through the direct committer and through a handle on one lane thread.
    const N: u64 = 200;
    let time = |through: &mut dyn Log, index| {
        let start = std::time::Instant::now();
        for seq in 0..N {
            let out = through.commit(vec![req(index, seq, "x")]);
            assert!(matches!(out[0], hsearch_commit::Outcome::Committed(_)));
        }
        start.elapsed()
    };
    let store = Crash::new();
    let (lane, mut direct) = open_lane(&store);
    let direct_time = time(&mut direct, INDEX);
    drop(direct);
    let shared = lane_thread(&store, lane, 0);
    let mut handle = shared.handle();
    let shared_time = time(&mut handle, B);
    let ratio = shared_time.as_secs_f64() / direct_time.as_secs_f64();
    println!("shared/direct = {ratio:.2} (direct {direct_time:?}, shared {shared_time:?})");
    assert!(ratio < 10.0, "{ratio}");
}
