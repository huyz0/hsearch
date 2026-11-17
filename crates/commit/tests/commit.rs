//! The committer against the in-memory store with injected faults: what is acknowledged is durable
//! and in order, nothing follows a failed slot, and an unknown outcome is settled by reopening.
#[path = "../../lane/tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use hsearch_commit::{
    AnyDependencies, Committer, Dependencies, DependencyView, Outcome, Pending, Reject, Request,
    Resolution, Stale,
};
use hsearch_format::Id;
use hsearch_lane::batch::BatchPolicy;
use hsearch_lane::{LaneId, LaneWriter, block_on, create_lane, shared_group};
use hsearch_object::MetadataStore;
use hsearch_object::memory::Fault;
use support::*;

fn deps(n: u8) -> Dependencies {
    Dependencies {
        schema: [n; 32],
        crypto: [n; 32],
        resolved: [n; 32],
    }
}

fn put_req(index: Id, seq: u64, body: &[u8]) -> Request {
    Request {
        index,
        seq,
        body: body.to_vec(),
        delete: false,
        deps: deps(1),
    }
}

fn lane_with<S: MetadataStore>(store: &S, members: &[u32]) -> LaneId {
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

fn one_per_slot() -> BatchPolicy {
    BatchPolicy {
        max_records: 1,
        max_bytes: 4 << 20,
        max_wait: Duration::from_millis(1),
    }
}

fn committer<S: MetadataStore, D: DependencyView>(
    store: &Arc<S>,
    lane: LaneId,
    deps: D,
    policy: BatchPolicy,
) -> Committer<S, D> {
    let (writer, _) = block_on(LaneWriter::open(
        Arc::clone(store),
        layout(),
        lane,
        0,
        |_| Ok(()),
    ))
    .unwrap();
    Committer::new(writer, deps, policy).unwrap()
}

struct Current(u8);
impl DependencyView for Current {
    fn check(&self, _: &Id, d: &Dependencies) -> Result<(), Stale> {
        if d.schema != [self.0; 32] {
            Err(Stale::Schema)
        } else if d.crypto != [self.0; 32] {
            Err(Stale::Crypto)
        } else if d.resolved != [self.0; 32] {
            Err(Stale::Resolved)
        } else {
            Ok(())
        }
    }
}

#[test]
fn receipts_are_in_order_with_batch_positions() {
    let store = counted();
    let lane = lane_with(&*store, &[1, 2]);
    let mut c = committer(&store, lane, AnyDependencies, BatchPolicy::default());
    let reqs = vec![
        put_req(id(1), 1, b"a"),
        put_req(id(2), 1, b"b"),
        put_req(id(1), 2, b"c"),
    ];
    let digests: Vec<_> = reqs.iter().map(Request::digest).collect();
    let out = block_on(c.commit(reqs));
    let receipts: Vec<_> = out
        .iter()
        .map(|o| match o {
            Outcome::Committed(r) => *r,
            other => panic!("not committed: {other:?}"),
        })
        .collect();
    assert!(receipts.iter().all(|r| r.ordinal == receipts[0].ordinal));
    assert_eq!(
        receipts
            .iter()
            .map(|r| r.batch_position)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        receipts
            .iter()
            .map(|r| r.request_digest)
            .collect::<Vec<_>>(),
        digests
    );
    let next = block_on(c.commit(vec![put_req(id(1), 3, b"d")]));
    let Outcome::Committed(r) = next[0] else {
        panic!()
    };
    assert_eq!(r.ordinal, receipts[0].ordinal + 1);
}

#[test]
fn a_stale_dependency_is_rejected_with_nothing_written() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let mut c = committer(&store, lane, Current(1), BatchPolicy::default());
    let before = store.requests();
    let mut stale = put_req(id(1), 1, b"x");
    stale.deps.crypto = [9; 32];
    let mut stale_schema = put_req(id(1), 2, b"x");
    stale_schema.deps.schema = [9; 32];
    let mut stale_resolved = put_req(id(1), 3, b"x");
    stale_resolved.deps.resolved = [9; 32];
    let out = block_on(c.commit(vec![stale, stale_schema, stale_resolved]));
    assert_eq!(
        out,
        vec![
            Outcome::Rejected(Reject::Stale(Stale::Crypto)),
            Outcome::Rejected(Reject::Stale(Stale::Schema)),
            Outcome::Rejected(Reject::Stale(Stale::Resolved)),
        ]
    );
    assert_eq!(
        store.requests(),
        before,
        "a stale request must not reach the store"
    );
}

#[test]
fn only_the_requests_in_the_call_are_written_and_empty_calls_cost_nothing() {
    let store = counted();
    let lane = lane_with(&*store, &[1, 2]);
    let mut c = committer(&store, lane, AnyDependencies, BatchPolicy::default());
    let before = store.requests();
    assert!(block_on(c.commit(Vec::new())).is_empty());
    assert_eq!(store.requests(), before);
    let before = store.requests();
    block_on(c.commit(vec![put_req(id(1), 1, b"x")]));
    let after = store.requests();
    assert_eq!(after.since(&before).puts(), 1);
}

#[test]
fn bad_duplicate_and_unadmitted_requests_are_rejected_individually() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let mut c = committer(&store, lane, AnyDependencies, BatchPolicy::default());
    let mut delete_with_body = put_req(id(1), 5, b"x");
    delete_with_body.delete = true;
    let out = block_on(c.commit(vec![
        put_req(id(1), 1, b""),
        delete_with_body,
        put_req(id(1), 2, b"ok"),
        put_req(id(1), 2, b"again"),
        put_req(id(7), 1, b"stranger"),
        Request {
            index: id(1),
            seq: 3,
            body: Vec::new(),
            delete: true,
            deps: deps(1),
        },
    ]));
    assert_eq!(out[0], Outcome::Rejected(Reject::BadBody));
    assert_eq!(out[1], Outcome::Rejected(Reject::BadBody));
    assert!(matches!(out[2], Outcome::Committed(_)));
    assert_eq!(out[3], Outcome::Rejected(Reject::Duplicate));
    assert_eq!(out[4], Outcome::Rejected(Reject::NotAdmitted));
    assert!(matches!(out[5], Outcome::Committed(_)));
}

#[test]
fn nothing_follows_a_refused_slot_and_the_lane_stays_usable() {
    let store = Arc::new(Chaos::new(counted()));
    let lane = lane_with(&*store.inner, &[1]);
    let mut c = committer(&store, lane, AnyDependencies, one_per_slot());
    store.allow(1);
    let out = block_on(c.commit(vec![
        put_req(id(1), 1, b"a"),
        put_req(id(1), 2, b"b"),
        put_req(id(1), 3, b"c"),
    ]));
    assert!(matches!(out[0], Outcome::Committed(_)));
    assert_eq!(out[1], Outcome::Rejected(Reject::Store));
    assert_eq!(out[2], Outcome::NotAttempted);
    assert!(!c.is_poisoned());
    store.allow(i64::MAX);
    let out = block_on(c.commit(vec![put_req(id(1), 2, b"b")]));
    let Outcome::Committed(r) = out[0] else {
        panic!("{out:?}")
    };
    assert_eq!(r.ordinal, 2, "a refused slot burns no position");
}

#[test]
fn a_lost_reply_resolved_by_the_lane_is_committed() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let mut c = committer(&store, lane, AnyDependencies, BatchPolicy::default());
    store
        .inner()
        .inject(Fault::LoseResponseAfterCommit)
        .unwrap();
    let out = block_on(c.commit(vec![put_req(id(1), 1, b"x")]));
    assert!(matches!(out[0], Outcome::Committed(_)));
    assert!(!c.is_poisoned());
}

#[test]
fn an_unresolvable_reply_is_unknown_blocks_later_slots_and_recover_settles_it() {
    let store = Arc::new(Chaos::new(counted()));
    let lane = lane_with(&*store.inner, &[1]);
    let mut c = committer(&store, lane, AnyDependencies, one_per_slot());
    store
        .inner
        .inner()
        .inject(Fault::LoseResponseAfterCommit)
        .unwrap();
    store.fail_resolve.store(true, Ordering::SeqCst);
    let first = put_req(id(1), 1, b"maybe");
    let second = put_req(id(1), 2, b"later");
    let out = block_on(c.commit(vec![first.clone(), second.clone()]));
    assert_eq!(out[0], Outcome::Unknown { ordinal: 1 });
    assert_eq!(out[1], Outcome::NotAttempted);
    assert!(c.is_poisoned());
    assert_eq!(
        block_on(c.commit(vec![put_req(id(1), 3, b"z")])),
        vec![Outcome::Rejected(Reject::Poisoned)]
    );
    store.fail_resolve.store(false, Ordering::SeqCst);
    let pending = [Pending::of(&first), Pending::of(&second)];
    let (mut c2, _report, resolved) = block_on(Committer::recover(
        Arc::clone(&store),
        layout(),
        lane,
        0,
        AnyDependencies,
        one_per_slot(),
        &pending,
    ))
    .unwrap();
    let Resolution::Committed(r) = resolved[0] else {
        panic!("{resolved:?}")
    };
    assert_eq!((r.ordinal, r.request_digest), (1, first.digest()));
    assert_eq!(resolved[1], Resolution::NotCommitted);
    let out = block_on(c2.commit(vec![second]));
    let Outcome::Committed(r) = out[0] else {
        panic!("{out:?}")
    };
    assert!(r.ordinal > 1);
}

#[test]
fn a_slot_that_never_landed_resolves_as_not_committed() {
    let store = Arc::new(Chaos::new(counted()));
    let lane = lane_with(&*store.inner, &[1]);
    let mut c = committer(&store, lane, AnyDependencies, BatchPolicy::default());
    let req = put_req(id(1), 1, b"x");
    let (_c2, _, resolved) = block_on(Committer::recover(
        Arc::clone(&store),
        layout(),
        lane,
        0,
        AnyDependencies,
        BatchPolicy::default(),
        &[Pending::of(&req)],
    ))
    .unwrap();
    assert_eq!(resolved, vec![Resolution::NotCommitted]);
    // The old owner is now fenced: its append is refused and acknowledges nothing.
    let out = block_on(c.commit(vec![req]));
    assert_eq!(out, vec![Outcome::Rejected(Reject::Fenced)]);
}

#[test]
fn a_different_body_under_the_same_identity_is_not_a_commit() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let mut c = committer(&store, lane, AnyDependencies, BatchPolicy::default());
    block_on(c.commit(vec![put_req(id(1), 1, b"real")]));
    let impostor = put_req(id(1), 1, b"other");
    let (_c2, _, resolved) = block_on(Committer::recover(
        Arc::clone(&store),
        layout(),
        lane,
        0,
        AnyDependencies,
        BatchPolicy::default(),
        &[Pending::of(&impostor)],
    ))
    .unwrap();
    assert_eq!(resolved, vec![Resolution::NotCommitted]);
}

#[test]
fn rejections_between_batches_map_to_the_right_requests_and_positions_reset() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let mut c = committer(&store, lane, Current(1), two_per_slot());
    let mut stale = put_req(id(1), 9, b"s");
    stale.deps.schema = [7; 32];
    let out = block_on(c.commit(vec![
        put_req(id(1), 1, b"a"),
        stale,
        put_req(id(1), 2, b"b"),
        put_req(id(9), 1, b"stranger"),
        put_req(id(1), 3, b"c"),
        put_req(id(1), 4, b"d"),
        put_req(id(1), 5, b"e"),
    ]));
    let r = |o: &Outcome| match o {
        Outcome::Committed(r) => (r.seq, r.ordinal, r.batch_position),
        other => panic!("{other:?}"),
    };
    assert_eq!(r(&out[0]), (1, 1, 0));
    assert_eq!(out[1], Outcome::Rejected(Reject::Stale(Stale::Schema)));
    assert_eq!(r(&out[2]), (2, 1, 1));
    assert_eq!(out[3], Outcome::Rejected(Reject::NotAdmitted));
    assert_eq!(r(&out[4]), (3, 2, 0));
    assert_eq!(r(&out[5]), (4, 2, 1));
    assert_eq!(r(&out[6]), (5, 3, 0));
}

fn two_per_slot() -> BatchPolicy {
    BatchPolicy {
        max_records: 2,
        ..one_per_slot()
    }
}

#[test]
fn unknown_indexes_cost_one_descriptor_read_per_call() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let mut c = committer(&store, lane, AnyDependencies, BatchPolicy::default());
    let before = store.requests();
    let out = block_on(c.commit((10..20).map(|n| put_req(id(n), 1, b"x")).collect()));
    assert!(
        out.iter()
            .all(|o| *o == Outcome::Rejected(Reject::NotAdmitted))
    );
    assert_eq!(store.requests().since(&before).total(), 2);
}

#[test]
fn an_object_squatting_the_next_slot_is_never_acknowledged() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let mut c = committer(&store, lane, AnyDependencies, BatchPolicy::default());
    // A second writer takes the lane (its open fences the end); the first writer's slot is gone.
    let _other = committer(&store, lane, AnyDependencies, BatchPolicy::default());
    let out = block_on(c.commit(vec![put_req(id(1), 1, b"x")]));
    assert_eq!(out, vec![Outcome::Rejected(Reject::Fenced)]);
    assert!(c.is_poisoned());
}

#[test]
fn a_fenced_later_batch_stops_the_rest_of_the_call() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let mut c = committer(&store, lane, AnyDependencies, one_per_slot());
    let first = block_on(c.commit(vec![put_req(id(1), 1, b"a")]));
    assert!(matches!(first[0], Outcome::Committed(_)));
    let _other = committer(&store, lane, AnyDependencies, BatchPolicy::default());
    let out = block_on(c.commit(vec![put_req(id(1), 2, b"b"), put_req(id(1), 3, b"c")]));
    assert_eq!(
        out,
        vec![Outcome::Rejected(Reject::Fenced), Outcome::NotAttempted]
    );
}

#[test]
fn recover_settles_deletes_and_picks_the_matching_body_when_a_pair_repeats() {
    let store = counted();
    let lane = lane_with(&*store, &[1]);
    let mut c = committer(&store, lane, AnyDependencies, one_per_slot());
    let del = Request {
        index: id(1),
        seq: 1,
        body: Vec::new(),
        delete: true,
        deps: deps(1),
    };
    let first = put_req(id(1), 2, b"first");
    let again = put_req(id(1), 2, b"second");
    block_on(c.commit(vec![del.clone()]));
    block_on(c.commit(vec![first.clone()]));
    block_on(c.commit(vec![again.clone()]));
    let (_c, _, resolved) = block_on(Committer::recover(
        Arc::clone(&store),
        layout(),
        lane,
        0,
        AnyDependencies,
        BatchPolicy::default(),
        &[Pending::of(&del), Pending::of(&first), Pending::of(&again)],
    ))
    .unwrap();
    let ords: Vec<_> = resolved
        .iter()
        .map(|r| match r {
            Resolution::Committed(r) => r.ordinal,
            Resolution::NotCommitted => panic!("{resolved:?}"),
        })
        .collect();
    assert_eq!(ords, vec![1, 2, 3]);
}

#[test]
fn a_seal_record_is_not_a_delete() {
    let store = counted();
    let lane = lane_with(&*store, &[1, 2]);
    let (mut w, _) = block_on(LaneWriter::open(
        Arc::clone(&store),
        layout(),
        lane,
        0,
        |_| Ok(()),
    ))
    .unwrap();
    block_on(w.seal_index(id(1), 1)).unwrap();
    let del = Request {
        index: id(1),
        seq: 1,
        body: Vec::new(),
        delete: true,
        deps: deps(1),
    };
    let (_c, _, resolved) = block_on(Committer::recover(
        Arc::clone(&store),
        layout(),
        lane,
        0,
        AnyDependencies,
        BatchPolicy::default(),
        &[Pending::of(&del)],
    ))
    .unwrap();
    assert_eq!(resolved, vec![Resolution::NotCommitted]);
}
