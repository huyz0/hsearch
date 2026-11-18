//! Recovery past its in-memory bounds, with external payloads, and across a sealed index's
//! successor lane, against the in-memory store.
#[path = "support/payload.rs"]
mod payload;
mod support;

use hsearch_lane::block_on;
use hsearch_store::recover::{Expect, NotReady, ReplayLimits, Spill};
use payload::*;
use support::*;

fn plain() -> Spill {
    Spill::Refuse
}

fn lane_with(store: &std::sync::Arc<Crash>) -> hsearch_lane::LaneId {
    let (lane, c) = open_lane(store);
    drop(c);
    lane
}

// ---- external payloads ------------------------------------------------------------------------

#[test]
fn external_payloads_are_fetched_verified_and_applied_in_sequence_order() {
    let store = Crash::new();
    let lane = lane_with(&store);
    let small = b"small external document".to_vec();
    // Two ranged reads: 8 MiB and the rest.
    let large: Vec<u8> = (0..(12u32 << 20)).map(|i| (i % 251) as u8).collect();
    put_payload(&store, &INDEX, &[1; 16], &small);
    put_payload(&store, &INDEX, &[2; 16], &large);
    let mut w = raw_writer(&store, lane);
    block_on(w.append(&[
        inline(INDEX, 0, &doc(0)),
        external(INDEX, 1, [1; 16], &small),
        external(INDEX, 2, [2; 16], &large),
        inline(INDEX, 3, &doc(3)),
    ]))
    .unwrap();
    drop(w);
    let (p, t) = tally(&store);
    let (got, applied) = run_with(&p, &lane, big(), Expect::default(), &plain());
    let got = got.unwrap();
    assert_eq!(got.applied, 4);
    assert_eq!(
        applied,
        vec![(0, doc(0)), (1, small), (2, large), (3, doc(3))]
    );
    // One HEAD per document, one GET per 8 MiB of it: requests scale with bytes.
    assert_eq!(t.counts(), (2, 1 + 2), "2 HEADs, then 1 + 2 ranged GETs");
}

#[test]
fn an_unreadable_external_payload_blocks_readiness_by_name() {
    let body = b"the document".to_vec();
    let cases: [(&str, Prepare); 4] = [
        ("absent", Box::new(|_| {})),
        (
            "altered",
            Box::new(|s| put_payload(s, &INDEX, &[5; 16], b"the documenX")),
        ),
        (
            "short",
            Box::new(|s| put_payload(s, &INDEX, &[5; 16], b"the docum")),
        ),
        (
            "long",
            Box::new(|s| put_payload(s, &INDEX, &[5; 16], b"the document and more")),
        ),
    ];
    for (name, make) in cases {
        let store = Crash::new();
        let lane = lane_with(&store);
        make(&store);
        let mut w = raw_writer(&store, lane);
        block_on(w.append(&[
            inline(INDEX, 0, &doc(0)),
            external(INDEX, 1, [5; 16], &body),
        ]))
        .unwrap();
        drop(w);
        let p = publisher(&store);
        let (got, applied) = run_with(&p, &lane, big(), Expect::default(), &plain());
        assert_eq!(got.unwrap_err(), NotReady::Payload(1), "{name}");
        assert_eq!(
            applied,
            vec![(0, doc(0))],
            "{name}: nothing past it applied"
        );
    }
}

#[test]
fn an_external_document_over_the_bytes_bound_costs_no_request() {
    let store = Crash::new();
    let lane = lane_with(&store);
    // The object does not exist: a fetch would say Payload, so Limit proves none was made.
    let mut w = raw_writer(&store, lane);
    block_on(w.append(&[external(INDEX, 0, [6; 16], &vec![1u8; 4096])])).unwrap();
    drop(w);
    let (p, t) = tally(&store);
    let tight = ReplayLimits {
        bytes: 4095,
        ..big()
    };
    let (got, _) = run_with(&p, &lane, tight, Expect::default(), &plain());
    assert_eq!(got.unwrap_err(), NotReady::Limit);
    assert_eq!(t.counts().0, 0, "no HEAD for the payload");
}

#[test]
fn a_repeated_external_record_is_compared_with_what_was_applied() {
    let body = b"same either way".to_vec();
    let store = Crash::new();
    let lane = lane_with(&store);
    put_payload(&store, &INDEX, &[7; 16], &body);
    let mut w = raw_writer(&store, lane);
    // The same document inline and external, then external again: harmless duplicates.
    block_on(w.append(&[
        inline(INDEX, 0, &body),
        external(INDEX, 0, [7; 16], &body),
        external(INDEX, 0, [7; 16], &body),
    ]))
    .unwrap();
    let p = publisher(&store);
    let (got, applied) = run_with(&p, &lane, big(), Expect::default(), &plain());
    assert_eq!(got.unwrap().replayed, 1);
    assert_eq!(applied, vec![(0, body.clone())]);
    // A different document under the same sequence is corruption, not a duplicate.
    block_on(w.append(&[inline(INDEX, 0, b"other")])).unwrap();
    let (got, _) = run_with(&p, &lane, big(), Expect::default(), &plain());
    assert_eq!(got.unwrap_err(), NotReady::Conflict(0));
}

// ---- spill ------------------------------------------------------------------------------------

const N: u64 = 2600;

fn long_tail() -> (std::sync::Arc<Crash>, hsearch_lane::LaneId) {
    let store = Crash::new();
    let lane = lane_with(&store);
    let mut w = raw_writer(&store, lane);
    append_docs(&mut w, INDEX, 0, N);
    drop(w);
    (store, lane)
}

fn scratch(dir: &std::path::Path, records: u64) -> Spill {
    Spill::Scratch {
        dir: dir.to_path_buf(),
        ceiling: ReplayLimits {
            slots: 1_000_000,
            records,
            bytes: 1 << 30,
        },
    }
}

fn files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).unwrap().count()
}

#[test]
fn a_tail_over_the_record_bound_is_refused_unless_it_may_spill() {
    let (store, lane) = long_tail();
    let p = publisher(&store);
    let (got, _) = run_with(&p, &lane, limits(16), Expect::default(), &plain());
    assert_eq!(got.unwrap_err(), NotReady::Limit);
}

#[test]
fn a_spilled_tail_replays_the_whole_history_and_leaves_no_scratch_behind() {
    let (store, lane) = long_tail();
    let p = publisher(&store);
    let dir = scratch_dir("whole");
    let mut during = 0;
    let mut got = Vec::new();
    let r = hsearch_store::recover::recover_with(
        &p,
        &target(&lane, limits(16), Expect::default()),
        &hsearch_store::publish::AnyDependencies,
        &scratch(&dir, 1 << 20),
        |r| {
            got.push((r.seq, r.body.to_vec()));
            during = files(&dir);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(got, model(N));
    assert_eq!((r.applied, r.replayed, r.spilled), (N, N, N - 16));
    assert_eq!(
        during, 1,
        "the digests of a long tail live in one scratch file"
    );
    assert_eq!(files(&dir), 0, "removed on return");
    // The same limits without a spill need no file at all.
    let (small, _) = run_with(
        &p,
        &lane,
        limits(1 << 20),
        Expect::default(),
        &scratch(&dir, 1 << 20),
    );
    assert_eq!(small.unwrap().spilled, 0);
    assert_eq!(files(&dir), 0);
}

#[test]
fn a_conflicting_repeat_of_a_spilled_sequence_is_still_found() {
    // Sequence 100 is in the flushed part of the scratch file, 2590 in its unflushed buffer, 5 in memory.
    for (seq, flushed) in [(5u64, "memory"), (100, "file"), (2590, "buffer")] {
        let (store, lane) = long_tail();
        let mut w = raw_writer(&store, lane);
        block_on(w.append(&[inline(INDEX, seq, &doc(seq))])).unwrap();
        drop(w);
        let p = publisher(&store);
        let dir = scratch_dir("dup");
        let (ok, applied) = run_with(
            &p,
            &lane,
            limits(16),
            Expect::default(),
            &scratch(&dir, 1 << 20),
        );
        assert_eq!(
            ok.unwrap().replayed,
            N,
            "{flushed}: an identical repeat is a duplicate"
        );
        assert_eq!(applied.len() as u64, N);
        let mut w = raw_writer(&store, lane);
        block_on(w.append(&[inline(INDEX, seq, b"different")])).unwrap();
        drop(w);
        let (bad, _) = run_with(
            &p,
            &lane,
            limits(16),
            Expect::default(),
            &scratch(&dir, 1 << 20),
        );
        assert_eq!(bad.unwrap_err(), NotReady::Conflict(seq), "{flushed}");
        assert_eq!(files(&dir), 0);
    }
}

#[test]
fn a_spill_has_a_ceiling_and_a_scratch_that_cannot_be_used_is_named() {
    let (store, lane) = long_tail();
    let p = publisher(&store);
    let dir = scratch_dir("ceiling");
    let (got, _) = run_with(
        &p,
        &lane,
        limits(16),
        Expect::default(),
        &scratch(&dir, 1000),
    );
    assert_eq!(got.unwrap_err(), NotReady::Limit);
    assert_eq!(files(&dir), 0);
    let missing = dir.join("absent");
    let (got, _) = run_with(
        &p,
        &lane,
        limits(16),
        Expect::default(),
        &Spill::Scratch {
            dir: missing,
            ceiling: limits(1 << 20),
        },
    );
    assert!(matches!(got.unwrap_err(), NotReady::Scratch(_)));
}

// ---- sealed successors ------------------------------------------------------------------------

#[test]
fn a_sealed_index_is_followed_to_its_successor_without_an_expected_sequence() {
    let store = Crash::new();
    let lane = lane_with(&store);
    let mut w = raw_writer(&store, lane);
    append_docs(&mut w, INDEX, 0, 5);
    append_docs(&mut w, B, 0, 3);
    let moved = block_on(w.promote(INDEX, 4)).unwrap();
    let mut t = raw_writer(&store, moved.target);
    append_docs(&mut t, INDEX, 5, 9);
    drop((w, t));
    let p = publisher(&store);
    let (got, applied) = run_with(&p, &lane, big(), Expect::default(), &plain());
    let got = got.unwrap();
    assert_eq!((got.applied, got.followed), (9, 1));
    assert_eq!(applied, model(9));
    // The source alone ends at the seal; asking for more than it has is refused as before.
    let (short, _) = run_with(
        &p,
        &lane,
        big(),
        Expect {
            ordinal: 0,
            seq: 10,
        },
        &plain(),
    );
    assert_eq!(short.unwrap_err(), NotReady::Missing);
}

#[test]
fn a_chain_of_promotions_is_followed_to_its_end() {
    let store = Crash::new();
    let lane = lane_with(&store);
    let mut w = raw_writer(&store, lane);
    append_docs(&mut w, INDEX, 0, 2);
    let first = block_on(w.promote(INDEX, 1)).unwrap();
    let mut t = raw_writer(&store, first.target);
    append_docs(&mut t, INDEX, 2, 4);
    let second = block_on(t.promote(INDEX, 3)).unwrap();
    let mut u = raw_writer(&store, second.target);
    append_docs(&mut u, INDEX, 4, 6);
    drop((w, t, u));
    let p = publisher(&store);
    let (got, applied) = run_with(&p, &lane, big(), Expect::default(), &plain());
    let got = got.unwrap();
    assert_eq!((got.applied, got.followed), (6, 2));
    assert_eq!(applied, model(6));
}

#[test]
fn a_seal_whose_promotion_did_not_finish_ends_the_history_at_the_seal() {
    let store = Crash::new();
    let lane = lane_with(&store);
    let mut w = raw_writer(&store, lane);
    append_docs(&mut w, INDEX, 0, 5);
    block_on(w.seal_index(INDEX, 4)).unwrap();
    drop(w);
    let p = publisher(&store);
    let (got, applied) = run_with(&p, &lane, big(), Expect::default(), &plain());
    let got = got.unwrap();
    assert_eq!((got.applied, got.followed), (5, 0));
    assert_eq!(applied, model(5));
}

#[test]
fn a_hole_before_the_seal_or_in_the_successor_blocks_readiness() {
    // The seal says the last sequence is 9 but only 0..5 are in the log.
    let store = Crash::new();
    let lane = lane_with(&store);
    let mut w = raw_writer(&store, lane);
    append_docs(&mut w, INDEX, 0, 5);
    block_on(w.seal_index(INDEX, 9)).unwrap();
    drop(w);
    let p = publisher(&store);
    let (got, _) = run_with(&p, &lane, big(), Expect::default(), &plain());
    assert_eq!(
        got.unwrap_err(),
        NotReady::Gap {
            expected: 5,
            found: 9
        }
    );

    // The successor starts past the sequence the source left off at.
    let store = Crash::new();
    let lane = lane_with(&store);
    let mut w = raw_writer(&store, lane);
    append_docs(&mut w, INDEX, 0, 5);
    let moved = block_on(w.promote(INDEX, 4)).unwrap();
    let mut t = raw_writer(&store, moved.target);
    append_docs(&mut t, INDEX, 6, 8);
    drop((w, t));
    let p = publisher(&store);
    let (got, _) = run_with(&p, &lane, big(), Expect::default(), &plain());
    assert_eq!(
        got.unwrap_err(),
        NotReady::Gap {
            expected: 5,
            found: 6
        }
    );
}

#[test]
fn successor_history_spills_and_fetches_externals_like_any_other() {
    let store = Crash::new();
    let lane = lane_with(&store);
    let mut w = raw_writer(&store, lane);
    append_docs(&mut w, INDEX, 0, 3);
    let moved = block_on(w.promote(INDEX, 2)).unwrap();
    let mut t = raw_writer(&store, moved.target);
    let big_doc = vec![9u8; 100_000];
    put_payload(&store, &INDEX, &[4; 16], &big_doc);
    block_on(t.append(&[external(INDEX, 3, [4; 16], &big_doc)])).unwrap();
    append_docs(&mut t, INDEX, 4, 200);
    drop((w, t));
    let p = publisher(&store);
    let dir = scratch_dir("successor");
    let (got, applied) = run_with(
        &p,
        &lane,
        limits(8),
        Expect::default(),
        &scratch(&dir, 1 << 20),
    );
    let got = got.unwrap();
    assert_eq!((got.applied, got.followed, got.spilled), (200, 1, 192));
    assert_eq!(applied[3].1, big_doc);
    assert_eq!(applied[199], (199, doc(199)));
}

#[test]
fn a_record_after_the_seal_in_the_same_lane_is_corrupt_and_a_repeated_seal_is_not() {
    let store = Crash::new();
    let lane = lane_with(&store);
    let mut w = raw_writer(&store, lane);
    append_docs(&mut w, INDEX, 0, 3);
    let next = w.next_ordinal();
    drop(w);
    raw_slot(&store, &lane, next, &[seal(INDEX, 2), seal(INDEX, 2)]);
    let p = publisher(&store);
    let (got, _) = run_with(&p, &lane, big(), Expect::default(), &plain());
    assert_eq!(
        got.unwrap().applied,
        3,
        "the same seal twice is the same fact"
    );
    raw_slot(&store, &lane, next + 1, &[inline(INDEX, 3, &doc(3))]);
    let (got, _) = run_with(&p, &lane, big(), Expect::default(), &plain());
    assert_eq!(got.unwrap_err(), NotReady::Corrupt);

    let store = Crash::new();
    let lane = lane_with(&store);
    let mut w = raw_writer(&store, lane);
    append_docs(&mut w, INDEX, 0, 3);
    let next = w.next_ordinal();
    drop(w);
    raw_slot(&store, &lane, next, &[seal(INDEX, 2), seal(INDEX, 7)]);
    let (got, _) = run_with(
        &publisher(&store),
        &lane,
        big(),
        Expect::default(),
        &plain(),
    );
    assert_eq!(
        got.unwrap_err(),
        NotReady::Corrupt,
        "two different last sequences"
    );
}

/// `cargo test --release -p hsearch-store --test spill -- --ignored --nocapture bench`: what the
/// spill costs against an all-in-memory replay of the same log (in-memory store, so the slot reads
/// cost almost nothing and the difference is the digest array).
#[test]
#[ignore = "a benchmark, not a check"]
fn bench_spill_against_memory() {
    const TOTAL: u64 = 150_000;
    let store = Crash::new();
    let lane = lane_with(&store);
    let mut w = raw_writer(&store, lane);
    append_docs(&mut w, INDEX, 0, TOTAL);
    drop(w);
    let p = publisher(&store);
    let dir = scratch_dir("bench");
    let rss = || {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines().find(|l| l.starts_with("VmRSS")).map(|l| {
                    l.split_whitespace()
                        .nth(1)
                        .unwrap_or("0")
                        .parse::<u64>()
                        .unwrap_or(0)
                })
            })
            .unwrap_or(0)
    };
    for (name, records, spill) in [
        ("memory", TOTAL, plain()),
        ("spill-1k", 1000, scratch(&dir, TOTAL)),
        ("spill-0", 0, scratch(&dir, TOTAL)),
    ] {
        let mut best = std::time::Duration::MAX;
        let mut peak = 0;
        for _ in 0..5 {
            let before = rss();
            let t = std::time::Instant::now();
            let mut n = 0u64;
            let r = hsearch_store::recover::recover_with(
                &p,
                &target(&lane, limits(records), Expect::default()),
                &hsearch_store::publish::AnyDependencies,
                &spill,
                |_| {
                    n += 1;
                    Ok(())
                },
            )
            .unwrap();
            best = best.min(t.elapsed());
            peak = peak.max(rss().saturating_sub(before));
            assert_eq!((n, r.applied), (TOTAL, TOTAL));
        }
        println!(
            "{name:9} {TOTAL} records: best of 5 {best:?} ({:.0} ns/record), rss growth after run {peak} kB",
            best.as_nanos() as f64 / TOTAL as f64
        );
    }
}
