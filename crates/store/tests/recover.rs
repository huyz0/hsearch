//! Recovery from the object store alone, against the in-memory store: the committed log replays on
//! the base the head names, and every kind of broken history blocks readiness with a named reason.
mod support;

use std::sync::Arc;

use hsearch_commit::{AnyDependencies as AnyDeps, Committer, Receipt};
use hsearch_format::{PayloadRef, RecordOp, RecordSpec};
use hsearch_lane::{LaneId, LaneWriter, block_on, read_head};
use hsearch_object::{
    Metadata, MetadataStore, Mutation as Put, ObjectBackend, ObjectKey, Payload, Precondition,
    RequestId,
};
use hsearch_store::publish::{AnyDependencies, BaseSpec};
use hsearch_store::recover::{Expect, NotReady, Record, ReplayLimits, recover};
use support::*;

/// Interleave `n` records of A (seq 0..n, body "a<seq>") with a record of B every other time.
fn fill(c: &mut Committer<Crash, AnyDeps>, n: u64) -> Vec<Receipt> {
    let mut ledger = Vec::new();
    for i in 0..n {
        ledger.push(commit(c, req(INDEX, i, &format!("a{i}"))));
        if i % 2 == 0 {
            commit(c, req(B, i / 2, &format!("b{}", i / 2)));
        }
    }
    ledger
}

fn run(
    store: &Arc<Crash>,
    lane: &LaneId,
    limits: ReplayLimits,
    expect: Expect,
) -> (
    Result<hsearch_store::recover::Recovered, NotReady>,
    Vec<(u64, String)>,
) {
    // A fresh publisher: nothing from the writer's life is carried over.
    let p = publisher(store);
    let mut got = Vec::new();
    let result = recover(
        &p,
        &target(lane, limits, expect),
        &AnyDependencies,
        |r: &Record<'_>| {
            got.push((r.seq, String::from_utf8(r.body.to_vec()).unwrap()));
            Ok(())
        },
    );
    (result, got)
}

fn slot_key(lane: &LaneId, ordinal: u64) -> String {
    layout().lane_slot(
        &lane.domain,
        lane.incarnation,
        lane.activation,
        lane.lane,
        ordinal,
    )
}

fn expected(ledger: &[Receipt], from: u64) -> Vec<(u64, String)> {
    ledger
        .iter()
        .filter(|r| r.seq >= from)
        .map(|r| (r.seq, format!("a{}", r.seq)))
        .collect()
}

#[test]
fn recovery_replays_the_committed_tail_on_the_published_base_and_matches_the_ledger() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 10);
    let last = ledger.iter().map(|r| r.ordinal).max().unwrap();
    drop(c);
    with_commit("recover", |local, _, names| {
        let p = publisher(&store);
        let s = spec(cut(5), 10, 1);
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        let (head, version) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
        let lane_cover = ledger.iter().find(|r| r.seq == 5).unwrap().ordinal;
        p.activate(
            &at(&lane, &head, &version),
            &published,
            &s,
            lane_cover,
            &AnyDependencies,
        )
        .unwrap();
        let (result, got) = run(
            &store,
            &lane,
            big(),
            Expect {
                ordinal: last + 1,
                seq: 10,
            },
        );
        let recovered = result.unwrap();
        // The recovered records are exactly the ledger's records past the base, in order.
        assert_eq!(got, expected(&ledger, 5));
        assert_eq!(recovered.from_seq, 5);
        assert_eq!(recovered.applied, 10);
        assert_eq!(recovered.replayed, 5);
        assert_eq!(recovered.base.as_ref().unwrap().covered, 5);
        // Replay started at the head's cover, not at the beginning of the lane.
        assert_eq!(recovered.slots, recovered.tail - lane_cover);
        assert!(recovered.tail > last);
    });
}

#[test]
fn without_a_base_the_whole_log_replays_from_the_start() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 6);
    drop(c);
    let (result, got) = run(&store, &lane, big(), Expect::default());
    let recovered = result.unwrap();
    assert_eq!(got, expected(&ledger, 0));
    assert!(recovered.base.is_none());
    assert_eq!((recovered.from_seq, recovered.applied), (0, 6));
}

#[test]
fn a_lost_slot_blocks_readiness_where_the_log_would_otherwise_end_early() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 8);
    let last = ledger.iter().map(|r| r.ordinal).max().unwrap();
    drop(c);
    // Remove the slot of A's seq 4: the log now ends there and the later slots are unreachable.
    let hole = ledger.iter().find(|r| r.seq == 4).unwrap().ordinal;
    let key = ObjectKey::new(slot_key(&lane, hole).as_bytes()).unwrap();
    let info = block_on(store.inner_ref().head(&ctx(), &key)).unwrap();
    block_on(
        store
            .inner_ref()
            .remove_version(&ctx(), &key, &info.version),
    )
    .unwrap();
    let (result, _) = run(
        &store,
        &lane,
        big(),
        Expect {
            ordinal: last + 1,
            seq: 8,
        },
    );
    assert_eq!(result.unwrap_err(), NotReady::Missing);
    // Without the caller's knowledge of what was acknowledged, the log simply ends at the hole:
    // that is why the expectation exists, and the head's own tail is the floor.
    let (result, got) = run(&store, &lane, big(), Expect::default());
    assert_eq!(result.unwrap().applied, 4);
    assert_eq!(got.len(), 4);
}

#[test]
fn a_sequence_gap_a_conflict_and_a_duplicate_are_told_apart() {
    // Gap: seq 2 never committed while 3 did.
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    for seq in [0, 1, 3] {
        commit(&mut c, req(INDEX, seq, "x"));
    }
    drop(c);
    assert_eq!(
        run(&store, &lane, big(), Expect::default()).0.unwrap_err(),
        NotReady::Gap {
            expected: 2,
            found: 3
        }
    );

    // A duplicate with the same body is applied once.
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    for (seq, body) in [(0, "p"), (1, "q"), (1, "q"), (2, "r")] {
        commit(&mut c, req(INDEX, seq, body));
    }
    drop(c);
    let (result, got) = run(&store, &lane, big(), Expect::default());
    let recovered = result.unwrap();
    assert_eq!((recovered.applied, recovered.replayed), (3, 3));
    assert_eq!(got, vec![(0, "p".into()), (1, "q".into()), (2, "r".into())]);

    // The same identity with a different body is corruption, never last-writer-wins.
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    for (seq, body) in [(0, "p"), (1, "q"), (1, "DIFFERENT")] {
        commit(&mut c, req(INDEX, seq, body));
    }
    drop(c);
    assert_eq!(
        run(&store, &lane, big(), Expect::default()).0.unwrap_err(),
        NotReady::Conflict(1)
    );
}

#[test]
fn a_corrupt_slot_and_a_failing_apply_block_readiness() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 4);
    drop(c);
    let target_slot = ledger[2].ordinal;
    let key = ObjectKey::new(slot_key(&lane, target_slot).as_bytes()).unwrap();
    let info = block_on(store.inner_ref().head(&ctx(), &key)).unwrap();
    let mut bytes = {
        let part = block_on(store.inner_ref().read_range(
            &ctx(),
            &key,
            &info.version,
            hsearch_object::ByteRange::new(0, info.length).unwrap(),
        ))
        .unwrap();
        part.as_bytes().to_vec()
    };
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x10;
    let replace = Put {
        request_id: RequestId::new(b"corrupt").unwrap(),
        key,
        payload: Payload::new(&bytes).unwrap(),
        metadata: Metadata::new(b"").unwrap(),
        precondition: Precondition::Matches(info.version),
    };
    block_on(store.inner_ref().compare_exchange(&ctx(), &replace)).unwrap();
    assert_eq!(
        run(&store, &lane, big(), Expect::default()).0.unwrap_err(),
        NotReady::Corrupt
    );

    // An apply that fails is not skipped.
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    fill(&mut c, 3);
    drop(c);
    let p = publisher(&store);
    let err = recover(
        &p,
        &target(&lane, big(), Expect::default()),
        &AnyDependencies,
        |_| Err(()),
    )
    .unwrap_err();
    assert_eq!(err, NotReady::Apply);
}

#[test]
fn an_oversized_replay_is_refused_not_attempted() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    fill(&mut c, 20);
    drop(c);
    let slots = ReplayLimits { slots: 5, ..big() };
    let records = ReplayLimits {
        records: 5,
        ..big()
    };
    let bytes = ReplayLimits { bytes: 8, ..big() };
    for limits in [slots, records, bytes] {
        assert_eq!(
            run(&store, &lane, limits, Expect::default()).0.unwrap_err(),
            NotReady::Limit
        );
    }
}

#[test]
fn a_missing_head_a_wrong_activation_and_a_broken_base_are_not_ready() {
    let store = Crash::new();
    let ghost = LaneId::new([3; 16]);
    assert_eq!(
        run(&store, &ghost, big(), Expect::default()).0.unwrap_err(),
        NotReady::NoHead
    );
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 6);
    drop(c);
    let mut later = lane;
    later.activation = 2;
    assert_eq!(
        run(&store, &later, big(), Expect::default()).0.unwrap_err(),
        NotReady::Activation
    );
    with_commit("brokenbase", |local, _, names| {
        let p = publisher(&store);
        let s = spec(cut(3), 6, 1);
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        let (head, version) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
        let cover = ledger.iter().find(|r| r.seq == 3).unwrap().ordinal;
        p.activate(
            &at(&lane, &head, &version),
            &published,
            &s,
            cover,
            &AnyDependencies,
        )
        .unwrap();
        // Lose one of the base's files: committed history is no longer whole.
        let page_key = layout().manifest_page(&INDEX, &SHARD, 1, &published.root.pages[0].page);
        let page_key = ObjectKey::new(page_key.as_bytes()).unwrap();
        let info = block_on(store.inner_ref().head(&ctx(), &page_key)).unwrap();
        let page = block_on(store.inner_ref().read_range(
            &ctx(),
            &page_key,
            &info.version,
            hsearch_object::ByteRange::new(0, info.length).unwrap(),
        ))
        .unwrap();
        let page = published.root.verify_page(0, page.as_bytes()).unwrap();
        let file_key =
            ObjectKey::new(layout().payload(&INDEX, &page.entries[0].object).as_bytes()).unwrap();
        let file = block_on(store.inner_ref().head(&ctx(), &file_key)).unwrap();
        block_on(
            store
                .inner_ref()
                .remove_version(&ctx(), &file_key, &file.version),
        )
        .unwrap();
        let err = run(&store, &lane, big(), Expect::default()).0.unwrap_err();
        assert!(matches!(err, NotReady::Base(_)), "{err:?}");
    });
}

#[test]
fn a_seal_record_is_not_a_document_operation() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    commit(&mut c, req(INDEX, 0, "a0"));
    commit(&mut c, req(INDEX, 1, "a1"));
    drop(c);
    // The index leaves this lane at sequence 2: a seal record, not a delete of anything.
    let (mut w, _) = block_on(LaneWriter::open_with_head(
        Arc::clone(&store),
        layout(),
        lane,
        |_| Ok(()),
    ))
    .unwrap();
    block_on(w.seal_index(INDEX, 2)).unwrap();
    let (result, got) = run(&store, &lane, big(), Expect::default());
    let recovered = result.unwrap();
    assert_eq!(recovered.applied, 2);
    assert_eq!(got, vec![(0, "a0".into()), (1, "a1".into())]);
}

#[test]
fn a_delete_reaches_apply_as_a_delete_and_conflicts_with_a_put_of_the_same_sequence() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    commit(&mut c, req(INDEX, 0, "a0"));
    commit(&mut c, del(INDEX, 1));
    drop(c);
    let p = publisher(&store);
    let mut seen = Vec::new();
    let result = recover(
        &p,
        &target(&lane, big(), Expect::default()),
        &AnyDependencies,
        |r: &Record<'_>| {
            seen.push((r.seq, r.delete, r.body.len()));
            Ok(())
        },
    );
    assert_eq!(result.unwrap().applied, 2);
    assert_eq!(seen, vec![(0, false, 2), (1, true, 0)]);

    // A put and a delete of one sequence are different operations, not a harmless duplicate.
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    commit(&mut c, req(INDEX, 0, "x"));
    commit(&mut c, del(INDEX, 0));
    drop(c);
    assert_eq!(
        run(&store, &lane, big(), Expect::default()).0.unwrap_err(),
        NotReady::Conflict(0)
    );
}

#[test]
fn an_external_payload_that_is_absent_blocks_readiness_instead_of_being_skipped() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    commit(&mut c, req(INDEX, 0, "a0"));
    drop(c);
    let (mut w, _) = block_on(LaneWriter::open_with_head(
        Arc::clone(&store),
        layout(),
        lane,
        |_| Ok(()),
    ))
    .unwrap();
    block_on(w.append(&[RecordSpec {
        index: INDEX,
        seq: 1,
        op: RecordOp::Put,
        payload: PayloadRef::External {
            object: [9; 16],
            length: 10,
            digest: [3; 32],
        },
    }]))
    .unwrap();
    assert_eq!(
        run(&store, &lane, big(), Expect::default()).0.unwrap_err(),
        NotReady::Payload(1)
    );
}

#[test]
fn records_below_the_base_are_duplicates_and_not_applied() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 10);
    drop(c);
    with_commit("below", |local, _, names| {
        let p = publisher(&store);
        let s = spec(cut(5), 10, 1);
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        let (head, version) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
        // A cover below the base's own position: records 2..5 are read again and are duplicates.
        let lane_cover = ledger.iter().find(|r| r.seq == 2).unwrap().ordinal;
        p.activate(
            &at(&lane, &head, &version),
            &published,
            &s,
            lane_cover,
            &AnyDependencies,
        )
        .unwrap();
        let (result, got) = run(&store, &lane, big(), Expect::default());
        let recovered = result.unwrap();
        assert_eq!(
            (recovered.from_seq, recovered.applied, recovered.replayed),
            (5, 10, 5)
        );
        assert_eq!(got, expected(&ledger, 5));
    });
}

#[test]
fn a_checkpoint_that_reclaims_the_scan_ahead_of_it_is_not_taken_for_the_end_of_the_log() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 10);
    drop(c);
    with_commit("moved", |local, _, names| {
        let p = publisher(&store);
        let s = spec(cut(5), 10, 1);
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        let (head, version) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
        let c5 = ledger.iter().find(|r| r.seq == 5).unwrap().ordinal;
        p.activate(
            &at(&lane, &head, &version),
            &published,
            &s,
            c5,
            &AnyDependencies,
        )
        .unwrap();
        // While recovery reads the tail, another node checkpoints at 8 and reclaims what it
        // covers: the slot of seq 6 vanishes ahead of the scan.
        let hole = ledger.iter().find(|r| r.seq == 6).unwrap().ordinal;
        let c8 = ledger.iter().find(|r| r.seq == 8).unwrap().ordinal;
        let recover_p = publisher(&store);
        let mut moved = false;
        let result = recover(
            &recover_p,
            &target(&lane, big(), Expect::default()),
            &AnyDependencies,
            |r: &Record<'_>| {
                if r.seq == 5 && !moved {
                    moved = true;
                    let other = publisher(&store);
                    let s2 = BaseSpec {
                        previous: Some(&published.root),
                        ..spec(cut(8), 10, 2)
                    };
                    let again = other.publish(local, names, &s2, &AnyDependencies).unwrap();
                    let (h, v) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
                    other
                        .activate(&at(&lane, &h, &v), &again, &s2, c8, &AnyDependencies)
                        .unwrap();
                    let key = ObjectKey::new(slot_key(&lane, hole).as_bytes()).unwrap();
                    let info = block_on(store.inner_ref().head(&ctx(), &key)).unwrap();
                    block_on(
                        store
                            .inner_ref()
                            .remove_version(&ctx(), &key, &info.version),
                    )
                    .unwrap();
                }
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err(), NotReady::Missing);
    });
}
