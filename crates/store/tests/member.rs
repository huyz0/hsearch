//! An index joins its lane at create and leaves it at delete: the catalog binds the incarnation,
//! a deleted index stops pinning the cover, a recreated name meets none of the old records, and
//! reclaim follows the pinned set. In-memory store.
mod support;

use hsearch_commit::{AnyDependencies as AnyDeps, Committer};
use hsearch_format::Durability;
use hsearch_lane::{
    LaneId, block_on, create_catalog, create_head, create_lane, read_head, shared_group,
};
use hsearch_store::cover::SharedCover;
use hsearch_store::log::Log;
use hsearch_store::member::{self, Error};
use hsearch_store::pointer::BaseSource;
use hsearch_store::publish::AnyDependencies;
use hsearch_store::recover::{Expect, Record, Target, recover};
use hsearch_store::shared::SharedLane;
use support::*;

const CATALOG: hsearch_format::Id = [5; 16];

/// A lane with one other member and a catalog, and a writer for the lane.
fn world(
    store: &std::sync::Arc<Crash>,
) -> (LaneId, hsearch_lane::Catalog, Committer<Crash, AnyDeps>) {
    let (domain, group) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    block_on(create_lane(
        store.inner_ref(),
        &layout(),
        &lane,
        &policy(),
        group,
        1000,
        &[B],
    ))
    .unwrap();
    block_on(create_head(
        store.inner_ref(),
        &layout(),
        &lane,
        Durability::Object,
    ))
    .unwrap();
    let catalog = block_on(create_catalog(store.inner_ref(), &layout(), CATALOG, 4)).unwrap();
    (lane, catalog, writer(store, lane))
}

fn replayed(store: &std::sync::Arc<Crash>, lane: &LaneId, index: hsearch_format::Id) -> Vec<u64> {
    let mut seqs = Vec::new();
    let t = Target {
        index,
        base: BaseSource::Pointer,
        ..target(lane, big(), Expect::default())
    };
    recover(&publisher(store), &t, &AnyDependencies, |r: &Record<'_>| {
        seqs.push(r.seq);
        Ok(())
    })
    .unwrap();
    seqs
}

#[test]
fn create_binds_the_incarnation_and_joins_and_repeating_it_changes_nothing() {
    let store = Crash::new();
    let (lane, catalog, mut c) = world(&store);
    let io = store.inner_ref();
    let first = member::create(io, &layout(), &catalog, &lane, &policy(), "logs").unwrap();
    assert_eq!(first.incarnation, 1);
    assert_eq!(first.index, hsearch_store::served::index_id_in("logs", 1));
    assert_eq!(
        member::create(io, &layout(), &catalog, &lane, &policy(), "logs").unwrap(),
        first
    );
    // The lane accepts the new member's records.
    let r = commit(&mut c, req(first.index, 0, "x"));
    assert_eq!(replayed(&store, &lane, first.index), vec![0]);
    assert!(r.ordinal > 0);
}

#[test]
fn a_deleted_index_stops_pinning_and_its_recreate_replays_none_of_its_records() {
    let store = Crash::new();
    let (lane, catalog, mut c) = world(&store);
    let io = store.inner_ref();
    let old = member::create(io, &layout(), &catalog, &lane, &policy(), "logs").unwrap();
    commit(&mut c, req(old.index, 0, "x"));
    commit(&mut c, req(old.index, 1, "y"));
    let (h, _) = block_on(read_head(io, &layout(), &lane)).unwrap();
    let pins = SharedCover::new(lane, h.writer_epoch, 1000);
    pins.wrote(old.index, 1);
    assert!(pins.is_pinned(&old.index));
    member::delete(io, &layout(), &catalog, &mut c, &pins, "logs", 1).unwrap();
    assert!(!pins.is_pinned(&old.index));
    assert!(c.writer().members().binary_search(&old.index).is_err());
    // A deleted name cannot be deleted again and its writes are refused.
    member::delete(io, &layout(), &catalog, &mut c, &pins, "logs", 1).unwrap();
    assert!(matches!(
        member::delete(io, &layout(), &catalog, &mut c, &pins, "never", 0),
        Err(Error::NotFound)
    ));
    let late = block_on(c.commit(vec![req(old.index, 2, "z")]));
    assert!(
        matches!(late[0], hsearch_commit::Outcome::Rejected(_)),
        "{late:?}"
    );
    // The name created again is another identity on the same lane and meets nothing old.
    let again = member::create(io, &layout(), &catalog, &lane, &policy(), "logs").unwrap();
    // The generation counts the delete too: it rises, which is all an incarnation must do.
    assert!(again.incarnation > old.incarnation);
    assert_eq!(
        again.index,
        hsearch_store::served::index_id_in("logs", again.incarnation)
    );
    assert_ne!(again.index, old.index);
    assert_eq!(
        member::create(io, &layout(), &catalog, &lane, &policy(), "logs").unwrap(),
        again
    );
    assert_eq!(again.incarnation, 3);
    assert!(replayed(&store, &lane, again.index).is_empty());
    commit(&mut c, req(again.index, 0, "n"));
    assert_eq!(replayed(&store, &lane, again.index), vec![0]);
}

#[test]
fn reclaim_removes_what_the_cover_passed_and_never_what_a_pinned_index_needs() {
    let store = Crash::new();
    let (lane, catalog, mut c) = world(&store);
    let io = store.inner_ref();
    let m = member::create(io, &layout(), &catalog, &lane, &policy(), "logs").unwrap();
    let a = commit(&mut c, req(m.index, 0, "a"));
    let b = commit(&mut c, req(B, 0, "b"));
    let _ = commit(&mut c, req(B, 1, "b"));
    let (h, _) = block_on(read_head(io, &layout(), &lane)).unwrap();
    let pins = SharedCover::new(lane, h.writer_epoch, 1000);
    pins.wrote(m.index, a.ordinal);
    pins.wrote(B, b.ordinal);
    let p = publisher(&store);
    // Both pinned: the cover cannot pass the first record, so nothing is removed.
    let none = member::reclaim(&p, &pins, &mut c).unwrap();
    assert_eq!(none.removed, 0);
    assert_eq!(replayed(&store, &lane, m.index), vec![0]);
    // Both published: the cover reaches the end and everything below it goes.
    pins.published(&m.index, b.ordinal + 3);
    pins.published(&B, b.ordinal + 3);
    let some = member::reclaim(&p, &pins, &mut c).unwrap();
    assert!(some.removed >= 2, "{some:?}");
}

#[test]
fn leave_and_reclaim_go_through_the_lane_thread_for_every_handle() {
    let store = Crash::new();
    let (lane, catalog, c) = world(&store);
    let io = store.inner_ref();
    let m = member::create(io, &layout(), &catalog, &lane, &policy(), "logs").unwrap();
    let shared = SharedLane::start(c, std::time::Duration::ZERO, 16);
    let mut h1 = shared.handle();
    let mut h2 = shared.handle();
    assert!(
        h1.commit(vec![req(m.index, 0, "x")])
            .iter()
            .all(|o| matches!(o, hsearch_commit::Outcome::Committed(_)))
    );
    let (head, _) = block_on(read_head(io, &layout(), &lane)).unwrap();
    let pins = SharedCover::new(lane, head.writer_epoch, 1000);
    member::delete(io, &layout(), &catalog, &mut h1, &pins, "logs", 0).unwrap();
    // The other handle sees the seal: the lane is one writer.
    let late = h2.commit(vec![req(m.index, 1, "y")]);
    assert!(
        matches!(late[0], hsearch_commit::Outcome::Rejected(_)),
        "{late:?}"
    );
    let p = publisher(&store);
    member::reclaim(&p, &pins, &mut h2).unwrap();
    assert!(!h2.is_poisoned());
}

#[test]
fn a_tail_index_that_does_not_publish_stays_pinned_and_holds_the_cover() {
    let store = Crash::new();
    let (lane, catalog, mut c) = world(&store);
    let m = member::create(
        store.inner_ref(),
        &layout(),
        &catalog,
        &lane,
        &policy(),
        "logs",
    )
    .unwrap();
    let a = commit(&mut c, req(m.index, 0, "a"));
    let b = commit(&mut c, req(B, 0, "b"));
    drop(c);
    let p = publisher(&store);
    let (h, _) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
    let pins = SharedCover::open(&p, lane, h.writer_epoch, 1000, 1000, |_| (SHARD, 1)).unwrap();
    assert!(pins.is_pinned(&m.index) && pins.is_pinned(&B));
    // B's host publishes; A's failed to open: it stays pinned and the cover stays at its record.
    let end = b.ordinal + 1;
    let done = member::settle_tail(&pins, |index| {
        let ok = *index == B;
        if ok {
            pins.published(index, end);
        }
        ok
    });
    assert_eq!(done, 1);
    assert!(pins.is_pinned(&m.index) && !pins.is_pinned(&B));
    assert_eq!(pins.allowed(), a.ordinal);
    // With one span bound of zero every pin is overdue, and a publish releases it.
    let late = SharedCover::new(lane, h.writer_epoch, 0);
    late.wrote(m.index, a.ordinal);
    let n = member::settle_overdue(&late, |index| {
        late.published(index, end);
        true
    });
    assert_eq!(n, 1);
    assert!(!late.is_pinned(&m.index));
}

#[test]
fn a_delete_that_stopped_after_the_catalog_is_finished_by_repeating_it_and_a_create_never_rejoins()
{
    let store = Crash::new();
    let (lane, catalog, mut c) = world(&store);
    let io = store.inner_ref();
    let old = member::create(io, &layout(), &catalog, &lane, &policy(), "logs").unwrap();
    commit(&mut c, req(old.index, 0, "x"));
    let (h, _) = block_on(read_head(io, &layout(), &lane)).unwrap();
    let pins = SharedCover::new(lane, h.writer_epoch, 1000);
    pins.wrote(old.index, 1);
    // The delete stopped after the tombstone: the lane still holds the member and the pin stays.
    block_on(catalog.delete(io, &layout(), "logs", &old.index)).unwrap();
    assert!(pins.is_pinned(&old.index));
    // A create now is another identity; it does not touch the old member.
    let again = member::create(io, &layout(), &catalog, &lane, &policy(), "logs").unwrap();
    assert_ne!(again.index, old.index);
    assert!(c.writer().members().binary_search(&old.index).is_ok());
    // Repeating the delete of the old identity's name is not possible once the name is live again,
    // so the lane step is finished through the leave the delete would have made.
    block_on(c.leave(old.index, 0)).unwrap();
    pins.forget(&old.index);
    assert!(c.writer().members().binary_search(&old.index).is_err());
    assert!(c.writer().members().binary_search(&again.index).is_ok());
    // The same leave repeated after it finished changes nothing.
    block_on(c.leave(old.index, 0)).unwrap();
}

#[test]
fn a_repeated_delete_after_the_tombstone_finishes_the_lane_step() {
    let store = Crash::new();
    let (lane, catalog, mut c) = world(&store);
    let io = store.inner_ref();
    let old = member::create(io, &layout(), &catalog, &lane, &policy(), "logs").unwrap();
    commit(&mut c, req(old.index, 0, "x"));
    let (h, _) = block_on(read_head(io, &layout(), &lane)).unwrap();
    let pins = SharedCover::new(lane, h.writer_epoch, 1000);
    pins.wrote(old.index, 1);
    block_on(catalog.delete(io, &layout(), "logs", &old.index)).unwrap();
    member::delete(io, &layout(), &catalog, &mut c, &pins, "logs", 0).unwrap();
    assert!(!pins.is_pinned(&old.index));
    assert!(c.writer().members().binary_search(&old.index).is_err());
}
