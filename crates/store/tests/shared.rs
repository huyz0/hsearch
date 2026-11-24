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
        cover: None,
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
    lane_thread_with(store, lane, wait_ms, hsearch_commit::AnyDependencies)
}

fn lane_thread_with<D: hsearch_commit::DependencyView + Send + 'static>(
    store: &Arc<Crash>,
    lane: LaneId,
    wait_ms: u64,
    view: D,
) -> SharedLane {
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
    let committer = hsearch_commit::Committer::new(w, view, policy).unwrap();
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
    // The flag, not b's own write, is what breaks b.
    assert!(shared.handle().is_poisoned());
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
    assert!(ratio < 100.0, "{ratio}");
}

#[test]
fn one_hosts_bad_request_does_not_touch_the_record_another_host_packed_beside_it() {
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    let shared = lane_thread(&store, lane, 500);
    let barrier = Arc::new(Barrier::new(2));
    let spawn = |request: hsearch_commit::Request| {
        let mut handle = shared.handle();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            handle.commit(vec![request])
        })
    };
    // A delete that carries a body is refused as a bad body; the good put beside it commits.
    let bad = hsearch_commit::Request {
        delete: true,
        ..req(INDEX, 0, "not allowed on a delete")
    };
    let bad = spawn(bad);
    let good = spawn(req(B, 0, "fine"));
    let bad = bad.join().unwrap().remove(0);
    let good = good.join().unwrap().remove(0);
    assert!(
        matches!(
            bad,
            hsearch_commit::Outcome::Rejected(hsearch_commit::Reject::BadBody)
        ),
        "{bad:?}"
    );
    assert!(
        matches!(good, hsearch_commit::Outcome::Committed(_)),
        "{good:?}"
    );
    assert!(!shared.handle().is_poisoned());
}

/// A dependency view that panics on one index, which ends the lane thread mid-call.
struct PanicOn(hsearch_format::Id);
impl hsearch_commit::DependencyView for PanicOn {
    fn check(
        &self,
        index: &hsearch_format::Id,
        _: &Dependencies,
    ) -> Result<(), hsearch_commit::Stale> {
        assert_ne!(*index, self.0, "planted lane thread failure");
        Ok(())
    }
}

#[test]
fn a_lane_thread_that_dies_leaves_unknown_outcomes_and_breaks_every_handle() {
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    let shared = lane_thread_with(&store, lane, 0, PanicOn(INDEX));
    let (mut a, mut b) = (shared.handle(), shared.handle());
    assert!(!b.is_poisoned());
    // The thread panics while checking the request: its reply is lost, never "refused".
    let lost = a.commit(vec![req(INDEX, 0, "x")]);
    assert!(
        matches!(lost[0], hsearch_commit::Outcome::Unknown { .. }),
        "{lost:?}"
    );
    assert!(b.is_poisoned());
    // Every later call on any handle is unknown too, not a refusal a caller could retry on.
    let later = b.commit(vec![req(B, 0, "y")]);
    assert!(
        matches!(later[0], hsearch_commit::Outcome::Unknown { .. }),
        "{later:?}"
    );
}

#[test]
fn hosts_pin_the_cover_until_each_publishes_and_the_restart_serves_what_was_acknowledged() {
    use hsearch_store::cover::SharedCover;
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    let shared = lane_thread(&store, lane, 0);
    let (h, _) =
        hsearch_lane::block_on(hsearch_lane::read_head(store.inner_ref(), &layout(), &lane))
            .unwrap();
    let pins = Arc::new(SharedCover::new(lane, h.writer_epoch, 1000));
    let with = |label: &str, index, seed| Config {
        cover: Some(Arc::clone(&pins)),
        ..config(label, index, 0, seed)
    };
    let a = open(&store, lane, &shared, with("ca", INDEX, 1));
    let b = open(&store, lane, &shared, with("cb", B, 2));
    for i in 0..4 {
        a.bulk(vec![put(&format!("a{i}"), "a")], Wait::Durable)
            .unwrap();
    }
    b.bulk(vec![put("b0", "b")], Wait::Durable).unwrap();
    let p = publisher(&store);
    // Both are pinned from their open: the cover cannot move.
    assert_eq!(pins.allowed(), 0);
    assert!(p.advance_cover(&pins).unwrap().is_none());
    // A publishes a base covering what it wrote; B has not, so B's pin still holds the cover.
    a.publish().unwrap();
    assert!(pins.is_pinned(&B) && !pins.is_pinned(&INDEX));
    assert!(pins.allowed() <= 6);
    assert!(p.advance_cover(&pins).unwrap().is_none());
    // B publishes too: nothing is pinned and the cover goes to the end of what was written.
    b.publish().unwrap();
    let end = pins.allowed();
    assert!(end > 4);
    // The host moved it itself when its base was named: one head read, no further move.
    assert!(p.advance_cover(&pins).unwrap().is_none());
    let (moved, _) =
        hsearch_lane::block_on(hsearch_lane::read_head(store.inner_ref(), &layout(), &lane))
            .unwrap();
    assert_eq!(moved.cover, end);
    // A base after the cover passed everything A wrote is still valid: nothing of A lies below it.
    a.publish().unwrap();
    // A write after its base pins its record again, and only from that record.
    a.bulk(vec![put("a4", "a")], Wait::Durable).unwrap();
    assert_eq!(pins.allowed(), end);
    let sequence = |index| {
        let spec = hsearch_store::publish::BaseSpec {
            index,
            ..spec(cut(1), 1, 1)
        };
        p.read_pointer(&spec).unwrap().unwrap().0.sequence
    };
    let before = (sequence(INDEX), sequence(B));
    b.stop();
    a.stop();
    // Stopping publishes only what is unpublished: B had nothing new, A had one write.
    assert_eq!((sequence(INDEX), sequence(B)), (before.0 + 1, before.1));
    // Stopping published A's last write: it no longer pins anything.
    assert!(pins.allowed() > end);
    let shared = lane_thread(&store, lane, 0);
    let a = open(&store, lane, &shared, config("ca2", INDEX, 0, 3));
    let b = open(&store, lane, &shared, config("cb2", B, 0, 4));
    for i in 0..5 {
        assert_eq!(tag(&a, &format!("a{i}")).as_deref(), Some("a"));
    }
    assert_eq!(tag(&b, "b0").as_deref(), Some("b"));
    assert_eq!(tag(&a, "b0"), None);
}

#[test]
fn a_host_whose_pin_has_held_its_bound_publishes_without_a_schedule() {
    use hsearch_store::cover::SharedCover;
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    let shared = lane_thread(&store, lane, 0);
    let pins = Arc::new(SharedCover::new(lane, 1, 3));
    let a = open(
        &store,
        lane,
        &shared,
        Config {
            cover: Some(Arc::clone(&pins)),
            ..config("span", INDEX, 0, 1)
        },
    );
    // No schedule (base_every_batches is 0), yet the pin from slot 0 reaches the bound of 3.
    for i in 0..5 {
        a.bulk(vec![put(&format!("s{i}"), "s")], Wait::Durable)
            .unwrap();
    }
    a.refresh().unwrap();
    assert!(a.stats().bases >= 1, "{:?}", a.stats());
    assert_eq!(a.stats().base_failures, 0);
}

#[test]
fn an_idle_host_never_pins_the_cover_and_a_stopped_one_publishes_what_it_pinned() {
    use hsearch_store::cover::SharedCover;
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    let shared = lane_thread(&store, lane, 0);
    let (h, _) =
        hsearch_lane::block_on(hsearch_lane::read_head(store.inner_ref(), &layout(), &lane))
            .unwrap();
    let pins = Arc::new(SharedCover::new(lane, h.writer_epoch, 1000));
    let with = |label: &str, index, seed| Config {
        cover: Some(Arc::clone(&pins)),
        ..config(label, index, 0, seed)
    };
    // B opens and never writes: it pins nothing, and stopping it is a no-op for the cover.
    let b = open(&store, lane, &shared, with("idle-b", B, 2));
    assert!(!pins.is_pinned(&B));
    let a = open(&store, lane, &shared, with("idle-a", INDEX, 1));
    a.bulk(vec![put("a0", "a")], Wait::Durable).unwrap();
    assert!(pins.is_pinned(&INDEX));
    b.stop();
    assert!(pins.is_pinned(&INDEX));
    // A stops: its pin is published away and nothing holds the cover.
    a.stop();
    assert!(!pins.is_pinned(&INDEX));
    // A restart that replays a tail pins from the tail and stops by publishing it.
    let again = open(&store, lane, &shared, config("idle-a2", INDEX, 0, 3));
    assert_eq!(tag(&again, "a0").as_deref(), Some("a"));
}
