//! The lane log, membership, placement and promotion against the in-memory store, with injected
//! faults. The same protocol runs against RustFS in `tests/rustfs.rs`.
mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use hsearch_format::{SlotKind, SlotView};
use hsearch_lane::counting::Requests;
use hsearch_lane::{
    Error, LaneId, LaneWriter, Placer, block_on, create_lane, dedicated_group, join_members,
    read_descriptor, read_log, shared_group,
};
use hsearch_object::memory::Fault;
use support::*;

fn lane_with<S: hsearch_object::MetadataStore>(store: &S, members: &[u32]) -> LaneId {
    let (domain, group_id) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    let members: Vec<_> = members.iter().map(|n| id(*n)).collect();
    block_on(create_lane(
        store,
        &layout(),
        &lane,
        &policy(),
        group_id,
        1000,
        &members,
    ))
    .unwrap();
    lane
}

fn open<S: hsearch_object::MetadataStore>(
    store: &Arc<S>,
    lane: LaneId,
    from: u64,
) -> (LaneWriter<S>, hsearch_lane::OpenReport) {
    block_on(LaneWriter::open(
        Arc::clone(store),
        layout(),
        lane,
        from,
        |_| Ok(()),
    ))
    .unwrap()
}

type Slot = (u64, SlotKind, Vec<(hsearch_format::Id, u64)>);

fn replay<S: hsearch_object::MetadataStore>(store: &S, lane: &LaneId, from: u64) -> Vec<Slot> {
    let mut out = Vec::new();
    block_on(read_log(
        store,
        &layout(),
        lane,
        from,
        None,
        |v: &SlotView<'_>| {
            out.push((
                v.position.ordinal,
                v.kind,
                v.records().map(|r| (r.index, r.seq)).collect(),
            ));
            Ok(())
        },
    ))
    .unwrap();
    out
}

#[test]
fn opening_seals_the_first_absent_position_and_every_open_seals_again() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let (writer, report) = open(&store, lane, 0);
    assert_eq!((report.fence_ordinal, writer.next_ordinal()), (0, 1));
    assert_eq!(report.slots_replayed, 0);
    drop(writer);
    let (writer, report) = open(&store, lane, 0);
    assert_eq!(
        report.fences_replayed, 1,
        "the first fence is retained and replayed"
    );
    assert_eq!((report.fence_ordinal, writer.next_ordinal()), (1, 2));
    let slots = replay(&*store, &lane, 0);
    assert_eq!(
        slots.iter().map(|s| s.1).collect::<Vec<_>>(),
        vec![SlotKind::Fence; 2]
    );
}

#[test]
fn a_batch_of_many_indexes_is_one_put_and_nothing_else() {
    let store = counted();
    let lane = lane_with(&*store, &[1, 2, 3]);
    let (mut writer, _) = open(&store, lane, 0);
    let before = store.requests();
    let body = [7u8; 100];
    let records = [
        put(id(1), 1, &body),
        put(id(2), 1, &body),
        put(id(3), 1, &body),
    ];
    let ack = block_on(writer.append(&records)).unwrap();
    assert_eq!((ack.ordinal, ack.records), (1, 3));
    let used = store.requests().since(&before);
    assert_eq!(
        used,
        Requests {
            create: 1,
            put_bytes: ack.bytes as u64,
            ..Requests::default()
        },
        "one conditional create for a whole batch: no head CAS, no read, no resolve"
    );
}

#[test]
fn replay_returns_slots_in_order_and_starts_at_the_cover() {
    let store = counted();
    let lane = lane_with(&*store, &[1, 2]);
    let (mut writer, _) = open(&store, lane, 0);
    for n in 0..5u64 {
        block_on(writer.append(&[put(id(1 + (n % 2) as u32), n, b"x")])).unwrap();
    }
    let all = replay(&*store, &lane, 0);
    assert_eq!(all.len(), 6);
    assert_eq!(
        all.iter().map(|s| s.0).collect::<Vec<_>>(),
        (0..6).collect::<Vec<_>>()
    );
    // A reopen from the cover replays only what is above it and costs two requests a slot.
    drop(writer);
    let before = store.requests();
    let mut seen = 0;
    let (_w, report) = block_on(LaneWriter::open(
        Arc::clone(&store),
        layout(),
        lane,
        4,
        |_| {
            seen += 1;
            Ok(())
        },
    ))
    .unwrap();
    assert_eq!(
        (seen, report.slots_replayed, report.fence_ordinal),
        (2, 2, 6)
    );
    let used = store.requests().since(&before);
    // descriptor 1+1, two slots 2+2, one HEAD at the end, one HEAD guarding against a hole, one fence.
    assert_eq!(
        (used.head, used.read, used.create),
        (1 + 2 + 1 + 1, 1 + 2, 1)
    );
}

#[test]
fn a_late_writer_is_refused_and_poisoned_and_its_successor_continues() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let (mut old, _) = open(&store, lane, 0);
    block_on(old.append(&[put(id(1), 1, b"before")])).unwrap();
    // The successor seals while the old writer is suspended before its next PUT.
    let (mut new, report) = open(&store, lane, 0);
    assert_eq!(report.records_replayed, 1);
    assert_eq!(
        new.next_ordinal(),
        old.next_ordinal() + 1,
        "the fence took the old writer's slot"
    );
    assert_eq!(
        block_on(old.append(&[put(id(1), 2, b"late")])).unwrap_err(),
        Error::Fenced
    );
    assert!(old.is_poisoned());
    assert_eq!(
        block_on(old.append(&[put(id(1), 3, b"x")])).unwrap_err(),
        Error::Poisoned
    );
    block_on(new.append(&[put(id(1), 2, b"after")])).unwrap();
    let seqs: Vec<_> = replay(&*store, &lane, 0)
        .into_iter()
        .flat_map(|s| s.2)
        .map(|r| r.1)
        .collect();
    assert_eq!(
        seqs,
        vec![1, 2],
        "the late write never landed; the successor's did"
    );
}

#[test]
fn a_lost_reply_is_resolved_by_identity_and_acknowledged_once() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let (mut writer, _) = open(&store, lane, 0);
    store
        .inner()
        .inject(Fault::LoseResponseAfterCommit)
        .unwrap();
    let ack = block_on(writer.append(&[put(id(1), 1, b"x")])).unwrap();
    assert_eq!(ack.ordinal, 1);
    assert!(!writer.is_poisoned());
    block_on(writer.append(&[put(id(1), 2, b"y")])).unwrap();
    assert_eq!(
        replay(&*store, &lane, 0)
            .iter()
            .filter(|s| s.1 == SlotKind::Data)
            .count(),
        2
    );
}

#[test]
fn an_unresolvable_reply_poisons_and_is_not_acknowledged_but_may_land() {
    let store = Arc::new(Chaos::new(counted()));
    let lane = lane_with(&*store.inner, &[1]);
    let (mut writer, _) = open(&store, lane, 0);
    store
        .inner
        .inner()
        .inject(Fault::LoseResponseAfterCommit)
        .unwrap();
    store.fail_resolve.store(true, Ordering::SeqCst);
    let error = block_on(writer.append(&[put(id(1), 1, b"maybe")])).unwrap_err();
    assert_eq!(error, Error::Unknown { ordinal: 1 });
    assert!(writer.is_poisoned());
    assert_eq!(
        block_on(writer.append(&[put(id(1), 2, b"x")])).unwrap_err(),
        Error::Poisoned
    );
    // Recovery is a reopen, and it sees the slot: durable although never acknowledged.
    store.fail_resolve.store(false, Ordering::SeqCst);
    let mut recovered = Vec::new();
    let (writer, _) = block_on(LaneWriter::open(
        Arc::clone(&store),
        layout(),
        lane,
        0,
        |v| {
            recovered.extend(v.records().map(|r| r.seq));
            Ok(())
        },
    ))
    .unwrap();
    assert_eq!(recovered, vec![1]);
    assert_eq!(writer.next_ordinal(), 3);
}

#[test]
fn a_certain_refusal_neither_poisons_nor_burns_a_position() {
    let store = Arc::new(Chaos::new(counted()));
    let lane = lane_with(&*store.inner, &[1]);
    let (mut writer, _) = open(&store, lane, 0);
    store.allow(0);
    assert_eq!(
        block_on(writer.append(&[put(id(1), 1, b"x")])).unwrap_err(),
        Error::Store(hsearch_object::Error::Unavailable)
    );
    assert!(!writer.is_poisoned());
    store.allow(i64::MAX);
    assert_eq!(
        block_on(writer.append(&[put(id(1), 1, b"x")]))
            .unwrap()
            .ordinal,
        1
    );
}

#[test]
fn only_members_that_are_not_sealed_may_write() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let (mut writer, _) = open(&store, lane, 0);
    assert_eq!(
        block_on(writer.append(&[put(id(9), 1, b"x")])).unwrap_err(),
        Error::NotMember(id(9))
    );
    assert!(matches!(
        block_on(writer.append(&[hsearch_format::RecordSpec {
            index: id(1),
            seq: 1,
            op: hsearch_format::RecordOp::Seal,
            payload: hsearch_format::PayloadRef::None,
        }])),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        block_on(writer.append(&[])).unwrap_err(),
        Error::Invalid("batch size")
    );
    // A joiner is seen after one descriptor re-read.
    block_on(join_members(
        &*store,
        &layout(),
        &lane,
        &policy(),
        &[id(9)],
        None,
    ))
    .unwrap();
    let before = store.requests();
    block_on(writer.append(&[put(id(9), 1, b"x")])).unwrap();
    let used = store.requests().since(&before);
    assert_eq!(
        (used.head, used.read, used.create),
        (1, 1, 1),
        "re-read once, then one PUT"
    );
    block_on(writer.append(&[put(id(9), 2, b"x")])).unwrap();
    assert_eq!(store.requests().since(&before).create, 2);
    assert_eq!(store.requests().since(&before).head, 1, "no further reads");
}

#[test]
fn membership_obeys_policy_ceiling_and_the_byok_rule() {
    let store = counted();
    let (domain, group_id) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    block_on(create_lane(
        &*store,
        &layout(),
        &lane,
        &policy(),
        group_id,
        3,
        &[id(1)],
    ))
    .unwrap();
    assert_eq!(
        block_on(create_lane(
            &*store,
            &layout(),
            &lane,
            &policy(),
            group_id,
            3,
            &[id(1)]
        ))
        .unwrap_err(),
        Error::Exists
    );
    // Several joiners cost one CAS.
    let before = store.requests();
    block_on(join_members(
        &*store,
        &layout(),
        &lane,
        &policy(),
        &[id(2), id(3)],
        None,
    ))
    .unwrap();
    assert_eq!(store.requests().since(&before).cas, 1);
    assert_eq!(
        block_on(join_members(
            &*store,
            &layout(),
            &lane,
            &policy(),
            &[id(4)],
            None
        ))
        .unwrap_err(),
        Error::Exists,
        "a full group refuses"
    );
    assert_eq!(
        block_on(join_members(
            &*store,
            &layout(),
            &lane,
            &byok("k"),
            &[id(1)],
            None
        ))
        .unwrap(),
        block_on(read_descriptor(&*store, &layout(), &lane)).unwrap(),
        "an existing member is a no-op even under another tuple"
    );
    let other = byok("kms/key-1");
    let (d2, g2) = dedicated_group(&other, &id(1), b"byok");
    assert!(
        block_on(create_lane(
            &*store,
            &layout(),
            &LaneId::new(d2),
            &other,
            g2,
            1000,
            &[id(1), id(2)]
        ))
        .is_err(),
        "a revocable key never wraps a shared lane"
    );
}

#[test]
fn placement_fills_groups_in_order_and_a_new_node_finds_the_open_group_without_listing() {
    let store = counted();
    let mut placer = Placer::with_ceiling(Arc::clone(&store), layout(), 10);
    let mut lanes = std::collections::BTreeSet::new();
    for n in 0..35u32 {
        let p = block_on(placer.place(&policy(), id(n))).unwrap();
        lanes.insert(p.lane.domain);
        assert_eq!(p.group, u64::from(n / 10));
        assert_eq!(p.created, n % 10 == 0);
    }
    assert_eq!(lanes.len(), 4);
    // Placing the index the open group already holds again writes nothing; remembering which lane an
    // index lives in is the catalog's job (M03.05), so `place` is only for an index with no lane.
    let before = store.requests();
    assert_eq!(block_on(placer.place(&policy(), id(34))).unwrap().group, 3);
    assert_eq!(store.requests().since(&before).cas, 0);
    // A cold node finds group 3 by doubling and bisection, then joins with one CAS.
    let mut cold = Placer::with_ceiling(Arc::clone(&store), layout(), 10);
    let before = store.requests();
    let p = block_on(cold.place(&policy(), id(100))).unwrap();
    assert_eq!(p.group, 3);
    let used = store.requests().since(&before);
    assert_eq!(used.cas, 1, "one CAS to join");
    assert!(
        used.head <= 2 * 3 + 3,
        "doubling and bisecting, not a scan: {used:?}"
    );
    // A BYOK index is alone in its own lane, in one create.
    let k = byok("kms/a");
    let a = block_on(cold.place(&k, id(500))).unwrap();
    let b = block_on(cold.place(&k, id(501))).unwrap();
    assert_ne!(a.lane.domain, b.lane.domain);
    assert_eq!(
        block_on(read_descriptor(&*store, &layout(), &a.lane))
            .unwrap()
            .0
            .members,
        vec![id(500)]
    );
}

#[test]
fn promotion_seals_in_order_moves_membership_and_resumes_after_a_crash_at_every_step() {
    // Count the mutations a clean promotion needs, then crash after each prefix of them.
    let clean = Arc::new(Chaos::new(counted()));
    let lane = lane_with(&*clean.inner, &[1, 2]);
    let (mut w, _) = open(&clean, lane, 0);
    block_on(w.append(&[put(id(1), 1, b"a"), put(id(2), 1, b"b")])).unwrap();
    clean.allow(i64::MAX);
    let before = clean.inner.requests();
    let done = block_on(w.promote(id(1), 1)).unwrap();
    let used = clean.inner.requests().since(&before);
    assert_eq!(
        (used.create, used.cas),
        (2, 1),
        "seal slot, target descriptor, source leave"
    );
    assert_eq!(done.sealed_at, 2);
    assert_eq!(
        block_on(w.append(&[put(id(1), 2, b"no")])).unwrap_err(),
        Error::Sealed(id(1))
    );
    block_on(w.append(&[put(id(2), 2, b"mate")])).unwrap();
    let source = block_on(read_descriptor(&*clean.inner, &layout(), &lane))
        .unwrap()
        .0;
    assert_eq!(source.members, vec![id(2)]);
    let target = block_on(read_descriptor(&*clean.inner, &layout(), &done.target))
        .unwrap()
        .0;
    assert_eq!(target.members, vec![id(1)]);
    // X's records are all below its seal.
    let x: Vec<_> = replay(&*clean.inner, &lane, 0)
        .into_iter()
        .flat_map(|s| s.2.into_iter().map(move |r| (s.0, r)))
        .filter(|(_, r)| r.0 == id(1))
        .map(|(o, r)| (o, r.1))
        .collect();
    assert_eq!(
        x,
        vec![(1, 1), (2, 1)],
        "its put, then its seal at the position promote returned"
    );

    for crash_after in 0..3 {
        let store = Arc::new(Chaos::new(counted()));
        let lane = lane_with(&*store.inner, &[1, 2]);
        let (mut w, _) = open(&store, lane, 0);
        block_on(w.append(&[put(id(1), 1, b"a"), put(id(2), 1, b"b")])).unwrap();
        store.allow(crash_after);
        assert!(
            block_on(w.promote(id(1), 1)).is_err(),
            "crash after {crash_after}"
        );
        drop(w);
        store.allow(i64::MAX);
        let (mut w, _) = open(&store, lane, 0);
        let again = block_on(w.promote(id(1), 1)).unwrap();
        if crash_after > 0 {
            assert_eq!(
                again.target, done.target,
                "the seal survived the crash, so the target is the same"
            );
            assert_eq!(again.sealed_at, done.sealed_at);
        } else {
            assert!(
                again.sealed_at > done.sealed_at,
                "nothing was sealed, so a reopen seals later"
            );
        }
        let seals = replay(&*store.inner, &lane, 0)
            .iter()
            .flat_map(|s| s.2.clone())
            .filter(|r| r.0 == id(1) && r.1 == 1)
            .count();
        assert!(seals >= 1);
        let members = block_on(read_descriptor(&*store.inner, &layout(), &lane))
            .unwrap()
            .0
            .members;
        assert_eq!(members, vec![id(2)], "crash after {crash_after}");
        assert_eq!(
            block_on(read_descriptor(&*store.inner, &layout(), &again.target))
                .unwrap()
                .0
                .members,
            vec![id(1)]
        );
    }
}

#[test]
fn reclaim_removes_data_below_the_cover_never_a_fence_and_a_late_writer_is_still_refused() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let (mut old, _) = open(&store, lane, 0); // fence at 0
    for n in 0..4u64 {
        block_on(old.append(&[put(id(1), n, b"x")])).unwrap(); // 1..=4
    }
    // A successor seals at 5; the old writer is suspended before its PUT at 5.
    let (mut new, report) = open(&store, lane, 0);
    assert_eq!(report.fence_ordinal, 5);
    for n in 4..6u64 {
        block_on(new.append(&[put(id(1), n, b"y")])).unwrap(); // 6, 7
    }
    // The checkpoint covers everything below 7: data 1..=4 and 6 go, fences 0 and 5 stay.
    let done = block_on(new.reclaim_below(7)).unwrap();
    assert_eq!((done.removed, done.fences_kept, done.absent), (5, 2, 0));
    assert_eq!(
        block_on(new.reclaim_below(7)).unwrap(),
        hsearch_lane::Reclaimed::default(),
        "repeatable"
    );
    // Replay from the cover sees the fence at 5, then reaches the hole at 6 and stops: this is why
    // a reader or a successor starts at the durable cover, never below it.
    let kinds: Vec<_> = replay(&*store, &lane, 5)
        .into_iter()
        .map(|s| (s.0, s.1))
        .collect();
    assert_eq!(kinds, vec![(5, SlotKind::Fence)]);
    // The late PUT lands on the retained fence at 5 and is refused.
    assert_eq!(
        block_on(old.append(&[put(id(1), 99, b"late")])).unwrap_err(),
        Error::Fenced
    );
    // A reopen from the cover works: first absent position at or above it.
    drop(new);
    let (mut again, report) = open(&store, lane, 7);
    assert_eq!((report.slots_replayed, report.fence_ordinal), (1, 8));
    block_on(again.append(&[put(id(1), 7, b"z")])).unwrap();
    // A cover beyond the log is refused.
    assert!(block_on(again.reclaim_below(1000)).is_err());
}

#[test]
fn opening_below_the_reclaim_cover_is_refused_and_forks_nothing() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let (mut w, _) = open(&store, lane, 0);
    for n in 0..3u64 {
        block_on(w.append(&[put(id(1), n, b"x")])).unwrap(); // 1..=3
    }
    block_on(w.reclaim_below(3)).unwrap(); // data 1, 2 removed; fence 0 kept
    drop(w);
    // Starting at 1 reads the hole as the end of the log: slot 1 is absent but slot 2.. exist? 1 and
    // 2 are gone, 3 is present, so the hole is at 1 with slot 2 absent too; start at 2 instead,
    // where slot 3 lies directly above the hole.
    let err = block_on(LaneWriter::open(
        Arc::clone(&store),
        layout(),
        lane,
        2,
        |_| Ok(()),
    ))
    .err()
    .unwrap();
    assert_eq!(err, Error::BadCover);
    let before = store.requests();
    let (w, report) = open(&store, lane, 3);
    assert_eq!(report.fence_ordinal, 4);
    drop(w);
    assert_eq!(store.requests().since(&before).create, 1);
    // The refused open wrote nothing at the hole.
    let mut at_two = 0;
    let _ = block_on(read_log(&*store, &layout(), &lane, 2, Some(3), |_| {
        at_two += 1;
        Ok(())
    }));
    assert_eq!(at_two, 0);
}

#[test]
fn a_batch_naming_many_unknown_indexes_costs_one_descriptor_read() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let (mut writer, _) = open(&store, lane, 0);
    let unknown: Vec<_> = (100..140u32).map(|n| put(id(n), 1, b"x")).collect();
    let before = store.requests();
    assert_eq!(
        block_on(writer.append(&unknown)).unwrap_err(),
        Error::NotMember(id(100))
    );
    let used = store.requests().since(&before);
    assert_eq!(
        (used.head, used.read, used.create),
        (1, 1, 0),
        "one re-read, nothing written"
    );
}

#[test]
fn the_cover_may_not_pass_an_unfinished_promotion_and_a_poisoned_writer_reclaims_nothing() {
    let store = Arc::new(Chaos::new(counted()));
    let lane = lane_with(&*store.inner, &[1, 2]);
    let (mut w, _) = open(&store, lane, 0);
    block_on(w.append(&[put(id(1), 1, b"a"), put(id(2), 1, b"b")])).unwrap();
    // Crash after the seal, before the descriptor change: the seal is durable, X still a member.
    store.allow(1);
    assert!(block_on(w.promote(id(1), 1)).is_err());
    assert_eq!(w.unfinished_promotions(), vec![(id(1), 2)]);
    assert!(matches!(
        block_on(w.reclaim_below(3)),
        Err(Error::Invalid(_))
    ));
    // Finishing the promotion lifts the block.
    store.allow(i64::MAX);
    block_on(w.promote(id(1), 1)).unwrap();
    assert!(w.unfinished_promotions().is_empty());
    block_on(w.reclaim_below(3)).unwrap();
    // A fenced (poisoned) writer reclaims nothing.
    let (mut newer, _) = open(&store, lane, 3);
    assert_eq!(
        block_on(w.append(&[put(id(2), 2, b"late")])).unwrap_err(),
        Error::Fenced
    );
    assert_eq!(block_on(w.reclaim_below(4)).unwrap_err(), Error::Poisoned);
    assert_eq!(block_on(w.promote(id(2), 1)).unwrap_err(), Error::Poisoned);
    block_on(newer.append(&[put(id(2), 2, b"ok")])).unwrap();
}
