//! A shared lane's cover follows its members: it never passes a pinned record, an idle index whose
//! base covers what it wrote pins nothing, a restart reads only the tail, a deleted index leaves
//! the pinned set and a reused name never meets its old records. In-memory store.
mod support;

use hsearch_commit::Receipt;
use hsearch_lane::{LaneId, LaneWriter, block_on, read_head};
use hsearch_store::cover::LaneCover;
use hsearch_store::pointer::BaseSource;
use hsearch_store::publish::{AnyDependencies, Error as PublishError, LogStart};
use hsearch_store::recover::{Expect, NotReady, Record, Target, recover};
use hsearch_store::tail::{Standing, scan};
use support::*;

/// Interleave `n` records of A (body "a<seq>") and B (body "b<seq>"), A first.
fn fill(
    c: &mut hsearch_commit::Committer<Crash, hsearch_commit::AnyDependencies>,
    n: u64,
) -> Vec<(Receipt, Receipt)> {
    (0..n)
        .map(|i| {
            (
                commit(c, req(INDEX, i, &format!("a{i}"))),
                commit(c, req(B, i, &format!("b{i}"))),
            )
        })
        .collect()
}

fn head(
    store: &std::sync::Arc<Crash>,
    lane: &LaneId,
) -> (hsearch_format::StructuralHead, hsearch_object::VersionToken) {
    block_on(read_head(store.inner_ref(), &layout(), lane)).unwrap()
}

#[test]
fn the_cover_never_passes_a_pinned_record_and_stale_epochs_cannot_raise_it() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 6);
    let tail = ledger.last().unwrap().1.ordinal + 1;
    drop(c);
    let mut cover = LaneCover::new();
    // A's first uncovered record is its fourth; B published and pins nothing.
    cover.pin(INDEX, ledger[3].0.ordinal);
    assert_eq!(cover.allowed(tail), ledger[3].0.ordinal);
    let p = publisher(&store);
    let (h, v) = head(&store, &lane);
    let target = cover.allowed(tail);
    let (raised, _) = p
        .raise_cover(&at(&lane, &h, &v), target, h.writer_epoch)
        .unwrap();
    assert_eq!(raised.cover, ledger[3].0.ordinal);
    // A cover that is not above the head's changes nothing; another epoch's head is refused.
    let (h, v) = head(&store, &lane);
    assert!(matches!(
        p.raise_cover(&at(&lane, &h, &v), target, h.writer_epoch),
        Err(PublishError::Mismatch)
    ));
    assert!(matches!(
        p.raise_cover(&at(&lane, &h, &v), tail, h.writer_epoch + 1),
        Err(PublishError::Lost)
    ));
    // With A's base covering everything it wrote, nothing is pinned and the cover goes to the tail.
    cover.published(&INDEX, tail);
    assert_eq!(cover.pinned(), 0);
    assert_eq!(cover.allowed(tail), tail);
    let (raised, _) = p
        .raise_cover(&at(&lane, &h, &v), tail, h.writer_epoch)
        .unwrap();
    assert_eq!(raised.cover, tail);
}

#[test]
fn a_write_after_the_cut_keeps_the_index_pinned_from_the_cut() {
    let mut cover = LaneCover::new();
    cover.wrote(INDEX, 4);
    cover.wrote(INDEX, 9);
    // The base's cut was taken with the lane at 7: the write at 9 is not in it.
    cover.published(&INDEX, 7);
    assert_eq!(cover.lowest(), Some(7));
    cover.published(&INDEX, 10);
    assert_eq!(cover.lowest(), None);
}

#[test]
fn a_lightly_written_index_reaches_the_publish_bound() {
    let mut cover = LaneCover::new();
    assert!(!cover.span_reaches(&INDEX, 1000, 16));
    cover.wrote(INDEX, 5);
    assert!(!cover.span_reaches(&INDEX, 20, 16));
    assert!(cover.span_reaches(&INDEX, 21, 16));
}

#[test]
fn a_promoted_index_pins_its_source_until_its_new_lane_base_covers_the_seal() {
    let mut cover = LaneCover::new();
    cover.pin(INDEX, 3);
    // The seal named sequence 5: a new-lane base covering through 5 (covered 6) releases it, one
    // covering less does not.
    assert!(!cover.release_promoted(&INDEX, 5, 5));
    assert_eq!(cover.lowest(), Some(3));
    assert!(cover.release_promoted(&INDEX, 6, 5));
    assert_eq!(cover.lowest(), None);
    cover.pin(B, 8);
    cover.forget(&B);
    assert_eq!(cover.lowest(), None);
}

fn recover_a(
    store: &std::sync::Arc<Crash>,
    lane: &LaneId,
    index: hsearch_format::Id,
    expect: Expect,
) -> (
    Result<hsearch_store::recover::Recovered, NotReady>,
    Vec<u64>,
) {
    let mut seqs = Vec::new();
    let t = Target {
        index,
        base: BaseSource::Pointer,
        ..target(lane, big(), expect)
    };
    let r = recover(&publisher(store), &t, &AnyDependencies, |r: &Record<'_>| {
        seqs.push(r.seq);
        Ok(())
    });
    (r, seqs)
}

#[test]
fn an_idle_index_restarts_after_the_cover_and_reclaim_passed_its_pointer() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let first = fill(&mut c, 4);
    let after_a = first.last().unwrap().1.ordinal + 1;
    // A publishes a base covering its four records and goes idle; B keeps writing.
    let base = with_commit("cover-idle", |local, _, names| {
        publisher(&store)
            .publish(local, names, &spec(cut(4), 4, 1), &AnyDependencies)
            .unwrap()
    });
    let p = publisher(&store);
    let (h, _) = head(&store, &lane);
    p.activate_pointer(
        None,
        &LogStart {
            lane: &lane,
            lane_cover: first.last().unwrap().0.ordinal + 1,
            head_cover: h.cover,
        },
        &base,
        &spec(cut(4), 4, 1),
        &AnyDependencies,
    )
    .unwrap();
    for i in 4..10 {
        commit(&mut c, req(B, i, "more b"));
    }
    let tail = after_a + 6;
    drop(c);
    // Nothing is pinned (A published, B's pin is its own concern here), so the cover passes A's
    // pointer cover and the slots below it are reclaimed.
    let (mut w, _) = block_on(LaneWriter::open_with_head(
        store.clone(),
        layout(),
        lane,
        |_| Ok(()),
    ))
    .unwrap();
    let (h, v) = head(&store, &lane);
    p.raise_cover(&at(&lane, &h, &v), tail - 1, h.writer_epoch)
        .unwrap();
    let reclaimed = block_on(w.reclaim_to_checkpoint()).unwrap();
    assert!(reclaimed.removed > 0);
    drop(w);
    let (got, seqs) = recover_a(&store, &lane, INDEX, Expect::default());
    let got = got.unwrap();
    assert_eq!((got.from_seq, got.applied), (4, 4));
    assert!(seqs.is_empty());
}

#[test]
fn the_tail_scan_finds_only_indexes_with_records_their_base_does_not_cover() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 3);
    drop(c);
    let shard_of = |_: &hsearch_format::Id| (SHARD, 1u64);
    let p = publisher(&store);
    let tail = scan(&p, &lane, 1000, shard_of).unwrap();
    // No pointers yet: both indexes have uncovered records, from their first.
    assert_eq!(tail.indexes.len(), 2);
    let a = tail.indexes.iter().find(|t| t.index == INDEX).unwrap();
    assert_eq!(
        (a.first, a.covered, a.standing),
        (ledger[0].0.ordinal, 0, Standing::Member)
    );
    // Six records and the opening writer's fence.
    assert_eq!(tail.slots, tail.end);
    // A's base covers its first two records: its uncovered ones start at the third.
    let base = with_commit("cover-tail", |local, _, names| {
        p.publish(local, names, &spec(cut(2), 3, 1), &AnyDependencies)
            .unwrap()
    });
    let (h, _) = head(&store, &lane);
    p.activate_pointer(
        None,
        &LogStart {
            lane: &lane,
            lane_cover: ledger[2].0.ordinal,
            head_cover: h.cover,
        },
        &base,
        &spec(cut(2), 3, 1),
        &AnyDependencies,
    )
    .unwrap();
    let partial = scan(&p, &lane, 1000, shard_of).unwrap();
    let a = partial.indexes.iter().find(|t| t.index == INDEX).unwrap();
    assert_eq!((a.first, a.covered), (ledger[2].0.ordinal, 2));
    // Raise the head's cover to the end: a restart now reads no slot and finds nothing.
    let (h, v) = head(&store, &lane);
    p.raise_cover(&at(&lane, &h, &v), tail.end, h.writer_epoch)
        .unwrap();
    let again = scan(&p, &lane, 1000, shard_of).unwrap();
    assert_eq!((again.slots, again.indexes.len()), (0, 0));
}

#[test]
fn a_deleted_index_leaves_the_tail_set_and_a_promoted_one_stays_pinned() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    fill(&mut c, 3);
    drop(c);
    let (mut w, _) = block_on(LaneWriter::open_with_head(
        store.clone(),
        layout(),
        lane,
        |_| Ok(()),
    ))
    .unwrap();
    // B is deleted: sealed at its last sequence, then removed from the members.
    block_on(w.seal_index(B, 2)).unwrap();
    block_on(w.remove_member(&B)).unwrap();
    // A is promoted to a lane of its own.
    block_on(w.promote(INDEX, 2)).unwrap();
    drop(w);
    let tail = scan(&publisher(&store), &lane, 1000, |_| (SHARD, 1)).unwrap();
    let kinds: Vec<_> = tail.indexes.iter().map(|t| (t.index, t.standing)).collect();
    assert_eq!(kinds, vec![(INDEX, Standing::Promoted)]);
}

#[test]
fn a_tail_over_its_bound_is_refused() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    fill(&mut c, 5);
    drop(c);
    let r = scan(&publisher(&store), &lane, 4, |_| (SHARD, 1));
    assert!(matches!(r, Err(hsearch_store::tail::Error::Limit)), "{r:?}");
}

#[test]
fn a_name_reused_after_a_delete_never_meets_its_old_records() {
    let first = hsearch_store::served::index_id_in("logs", 1);
    let second = hsearch_store::served::index_id_in("logs", 2);
    assert_ne!(first, second);
    assert_ne!(first, hsearch_store::served::index_id_in("other", 1));
    assert_eq!(first, hsearch_store::served::index_id_in("logs", 1));
    // Recovery of the reused name filters the lane by identity: the old records are not replayed.
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    fill(&mut c, 3);
    drop(c);
    let (got, seqs) = recover_a(&store, &lane, second, Expect::default());
    let got = got.unwrap();
    assert_eq!((got.applied, got.replayed), (0, 0));
    assert!(seqs.is_empty());
}

#[test]
fn requests_per_acknowledged_write_do_not_grow_with_the_members_of_the_lane() {
    use hsearch_lane::{create_head, create_lane, shared_group};
    for members in [1usize, 100, 1000] {
        let store = Crash::new();
        let (domain, group) = shared_group(&policy(), 0);
        let lane = LaneId::new(domain);
        let ids: Vec<hsearch_format::Id> = {
            let mut v: Vec<_> = (0..members)
                .map(|i| {
                    let mut id = [0u8; 16];
                    id[..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
                    id
                })
                .collect();
            v.sort_unstable();
            v
        };
        block_on(create_lane(
            store.inner_ref(),
            &layout(),
            &lane,
            &policy(),
            group,
            4096,
            &ids,
        ))
        .unwrap();
        block_on(create_head(
            store.inner_ref(),
            &layout(),
            &lane,
            hsearch_format::Durability::Object,
        ))
        .unwrap();
        let mut c = writer(&store, lane);
        // The first call may refresh nothing: warm the writer, then count.
        commit(&mut c, req(ids[0], 0, "warm"));
        let before = store.writes();
        for seq in 1..=5 {
            commit(&mut c, req(ids[0], seq, "x"));
        }
        assert_eq!(store.writes() - before, 5, "{members} members");
    }
}
