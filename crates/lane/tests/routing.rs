//! M03.05: where a promoted index's writes go, derived by key, and never to two authorities.
mod support;

use std::sync::Arc;

use hsearch_lane::{
    Change, Error, LaneId, LaneWriter, Route, advance_head, block_on, create_lane, read_head,
    route, shared_group,
};
use support::*;

fn shared<S: hsearch_object::MetadataStore>(store: &S, members: &[u32]) -> LaneId {
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

fn open<S: hsearch_object::MetadataStore>(store: &Arc<S>, lane: LaneId) -> LaneWriter<S> {
    block_on(LaneWriter::open(
        Arc::clone(store),
        layout(),
        lane,
        0,
        |_| Ok(()),
    ))
    .unwrap()
    .0
}

#[test]
fn a_member_routes_to_its_shared_lane_a_stranger_to_nowhere() {
    let store = counted();
    let lane = shared(&*store, &[1, 2]);
    assert_eq!(
        block_on(route(&*store, &layout(), &lane, &id(1))).unwrap(),
        Route::Shared(lane)
    );
    assert_eq!(
        block_on(route(&*store, &layout(), &lane, &id(9))).unwrap(),
        Route::Unrouted
    );
}

#[test]
fn a_promoted_index_routes_to_its_own_lane_whose_head_a_writer_opens() {
    let store = counted();
    let lane = shared(&*store, &[1, 2]);
    let mut w = open(&store, lane);
    block_on(w.append(&[put(id(1), 1, b"a"), put(id(2), 1, b"b")])).unwrap();
    let done = block_on(w.promote(id(1), 1)).unwrap();
    let before = store.requests();
    let routed = block_on(route(&*store, &layout(), &lane, &id(1))).unwrap();
    let used = store.requests().since(&before);
    assert_eq!(
        routed,
        Route::Promoted {
            lane: done.target,
            writer_epoch: 0
        }
    );
    // Two reads of 1 HEAD and 1 GET each (descriptor, head), nothing written, nothing listed.
    assert_eq!((used.head, used.read), (2, 2));
    assert_eq!(used.create + used.cas + used.remove + used.resolve, 0);
    // The source refuses the index for good; a writer opens the target from its head.
    assert_eq!(
        block_on(w.append(&[put(id(1), 2, b"no")])).unwrap_err(),
        Error::Sealed(id(1))
    );
    let (mut t, report) = block_on(LaneWriter::open_with_head(
        Arc::clone(&store),
        layout(),
        done.target,
        |_| Ok(()),
    ))
    .unwrap();
    assert_eq!(report.head_epoch, 1);
    block_on(t.append(&[put(id(1), 2, b"c")])).unwrap();
    assert_eq!(
        block_on(route(&*store, &layout(), &lane, &id(1))).unwrap(),
        Route::Promoted {
            lane: done.target,
            writer_epoch: 1
        },
        "the head's newest epoch is reported"
    );
}

#[test]
fn a_new_activation_of_the_target_is_what_routing_reports() {
    let store = counted();
    let lane = shared(&*store, &[1]);
    let mut w = open(&store, lane);
    let done = block_on(w.promote(id(1), 0)).unwrap();
    let (h, v) = block_on(read_head(&*store, &layout(), &done.target)).unwrap();
    block_on(advance_head(
        &*store,
        &layout(),
        &done.target,
        &h,
        &v,
        Change::Establish {
            activation: 2,
            lane: 0,
            fence: 0,
        },
    ))
    .unwrap();
    match block_on(route(&*store, &layout(), &lane, &id(1))).unwrap() {
        Route::Promoted { lane: l, .. } => {
            assert_eq!((l.domain, l.activation), (done.target.domain, 2))
        }
        other => panic!("{other:?}"),
    }
    // The old activation has no authority (M03.05 head rule), the routed one does.
    let stale = block_on(LaneWriter::open_with_head(
        Arc::clone(&store),
        layout(),
        done.target,
        |_| Ok(()),
    ));
    assert_eq!(stale.err(), Some(Error::Fenced));
}

/// After a crash at any point of a promotion, routing never names two authorities for the index:
/// if it routes to the target, the source already refuses the index; if it routes to the source,
/// the target has no head or the index is still listed. The retry always finishes at the target.
#[test]
fn a_crash_at_every_step_of_a_promotion_never_leaves_two_authorities() {
    for crash_after in 0..5 {
        let store = Arc::new(Chaos::new(counted()));
        let lane = shared(&*store.inner, &[1, 2]);
        let mut w = open(&store, lane);
        block_on(w.append(&[put(id(1), 1, b"a"), put(id(2), 1, b"b")])).unwrap();
        store.allow(crash_after);
        let first = block_on(w.promote(id(1), 1));
        store.allow(i64::MAX);
        if crash_after < 4 {
            assert!(first.is_err(), "crash after {crash_after}");
        }
        let routed = block_on(route(&*store.inner, &layout(), &lane, &id(1))).unwrap();
        let source_accepts = block_on(w.append(&[put(id(1), 7, b"x")])).is_ok();
        match routed {
            Route::Promoted { .. } => assert!(
                !source_accepts,
                "routed to the target while the source still accepts (crash {crash_after})"
            ),
            Route::Shared(l) => assert_eq!(l, lane),
            Route::Unrouted => {}
        }
        drop(w);
        let mut w = open(&store, lane);
        let done = block_on(w.promote(id(1), 1)).unwrap();
        assert!(matches!(
            block_on(route(&*store.inner, &layout(), &lane, &id(1))).unwrap(),
            Route::Promoted { lane: l, .. } if l.domain == done.target.domain
        ));
    }
}
