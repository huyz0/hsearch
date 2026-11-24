//! A shard's own base pointer: the base and the replay start come from it rather than from the lane
//! head, a lost race or a lane cover below the head's changes nothing, and a lane with no pointer
//! recovers as before.
mod support;

use hsearch_commit::{AnyDependencies as AnyDeps, Committer, Receipt};
use hsearch_lane::{LaneId, block_on, read_head};
use hsearch_object::ObjectBackend as _;
use hsearch_store::pointer::{self, BaseSource};
use hsearch_store::publish::{AnyDependencies, Error as PublishError, LogStart, Published};
use hsearch_store::recover::{Expect, NotReady, Record, Recovered, Target, recover};
use support::*;

fn start(lane: &LaneId, lane_cover: u64, head_cover: u64) -> LogStart<'_> {
    LogStart {
        lane,
        lane_cover,
        head_cover,
    }
}

fn fill(c: &mut Committer<Crash, AnyDeps>, n: u64) -> Vec<Receipt> {
    (0..n)
        .map(|i| {
            let r = commit(c, req(INDEX, i, &format!("a{i}")));
            commit(c, req(B, i, &format!("b{i}")));
            r
        })
        .collect()
}

fn recover_from(
    store: &std::sync::Arc<Crash>,
    lane: &LaneId,
    base: BaseSource,
    expect: Expect,
) -> (Recovered, Vec<u64>) {
    let mut seqs = Vec::new();
    let target = Target {
        base,
        ..target(lane, big(), expect)
    };
    let recovered = recover(
        &publisher(store),
        &target,
        &AnyDependencies,
        |r: &Record<'_>| {
            seqs.push(r.seq);
            Ok(())
        },
    )
    .unwrap();
    (recovered, seqs)
}

fn published(store: &std::sync::Arc<Crash>, covered: u64, label: &str, attempt: u8) -> Published {
    let p = publisher(store);
    with_commit(label, |local, _, names| {
        p.publish(
            local,
            names,
            &spec(cut(covered), 10, attempt),
            &AnyDependencies,
        )
        .unwrap()
    })
}

#[test]
fn a_pointer_names_the_base_and_starts_the_replay_where_its_lane_cover_says() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 10);
    let last = ledger.iter().map(|r| r.ordinal).max().unwrap();
    drop(c);
    let base = published(&store, 5, "ptr-names", 1);
    let p = publisher(&store);
    let s = spec(cut(5), 10, 1);
    let lane_cover = ledger.iter().find(|r| r.seq == 5).unwrap().ordinal;
    let (head, _) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
    assert_eq!(head.checkpoint, None);
    let written = p
        .activate_pointer(
            None,
            &start(&lane, lane_cover, head.cover),
            &base,
            &s,
            &AnyDependencies,
        )
        .unwrap();
    assert_eq!((written.sequence, written.covered), (1, 5));
    assert_eq!(p.read_pointer(&s).unwrap().unwrap().0, written);
    // The head still names nothing: only the pointer says where the base is.
    let (after, _) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
    assert_eq!(after.checkpoint, None);
    let expect = Expect {
        ordinal: last + 1,
        seq: 10,
    };
    let (got, seqs) = recover_from(&store, &lane, BaseSource::Pointer, expect);
    assert_eq!(seqs, (5..10).collect::<Vec<_>>());
    assert_eq!((got.from_seq, got.applied, got.replayed), (5, 10, 5));
    assert_eq!(got.base.as_ref().unwrap().covered, 5);
    // The scan began at the pointer's lane cover, above the head's cover of zero.
    assert_eq!(got.slots, got.tail - lane_cover);
    // Reading the head instead ignores the pointer and replays the whole log.
    let (all, seqs) = recover_from(&store, &lane, BaseSource::Head, expect);
    assert_eq!(seqs, (0..10).collect::<Vec<_>>());
    assert!(all.base.is_none());
    assert_eq!(all.slots, all.tail);
}

#[test]
fn a_shard_with_no_pointer_replays_the_whole_log_from_a_pointer_source() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    fill(&mut c, 4);
    drop(c);
    let (got, seqs) = recover_from(&store, &lane, BaseSource::Pointer, Expect::default());
    assert_eq!(seqs, (0..4).collect::<Vec<_>>());
    assert!(got.base.is_none());
}

#[test]
fn a_pointer_replaced_from_a_stale_read_loses_and_changes_nothing() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    fill(&mut c, 6);
    drop(c);
    let base = published(&store, 5, "ptr-stale", 1);
    let p = publisher(&store);
    let s = spec(cut(5), 10, 1);
    let first = p
        .activate_pointer(None, &start(&lane, 0, 0), &base, &s, &AnyDependencies)
        .unwrap();
    let stale = p.read_pointer(&s).unwrap().unwrap();
    // Another writer replaces it first.
    let s2 = spec(cut(5), 10, 2);
    let second = p
        .activate_pointer(
            Some(&stale),
            &start(&lane, 0, 0),
            &base,
            &s2,
            &AnyDependencies,
        )
        .unwrap();
    assert_eq!(second.sequence, first.sequence + 1);
    // The same stale read cannot replace it again: the compare-and-swap names the old version.
    let s3 = spec(cut(5), 10, 3);
    let lost = p.activate_pointer(
        Some(&stale),
        &start(&lane, 0, 0),
        &base,
        &s3,
        &AnyDependencies,
    );
    assert!(matches!(lost, Err(PublishError::Lost)), "{lost:?}");
    assert_eq!(p.read_pointer(&s).unwrap().unwrap().0, second);
    // Creating it again when it exists loses too.
    let again = p.activate_pointer(None, &start(&lane, 0, 0), &base, &s3, &AnyDependencies);
    assert!(
        matches!(
            again,
            Err(PublishError::Lost) | Err(PublishError::Pointer(_))
        ),
        "{again:?}"
    );
    assert_eq!(p.read_pointer(&s).unwrap().unwrap().0, second);
}

#[test]
fn a_lane_cover_below_the_lane_heads_is_refused() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    fill(&mut c, 6);
    drop(c);
    let base = published(&store, 5, "ptr-below", 1);
    let p = publisher(&store);
    let s = spec(cut(5), 10, 1);
    let refused = p.activate_pointer(None, &start(&lane, 3, 8), &base, &s, &AnyDependencies);
    match refused {
        Err(PublishError::Pointer(e)) => assert!(
            matches!(
                *e,
                pointer::Error::Refused(hsearch_format::PointerRefusal::BelowHeadCover)
            ),
            "{e:?}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(p.read_pointer(&s).unwrap().is_none());
}

#[test]
fn recovery_from_a_pointer_equals_recovery_from_the_head_on_a_dedicated_lane() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let ledger = fill(&mut c, 8);
    let last = ledger.iter().map(|r| r.ordinal).max().unwrap();
    drop(c);
    let base = published(&store, 4, "ptr-equal", 1);
    let p = publisher(&store);
    let s = spec(cut(4), 8, 1);
    let lane_cover = ledger.iter().find(|r| r.seq == 4).unwrap().ordinal;
    let (head, version) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
    p.activate(
        &at(&lane, &head, &version),
        &base,
        &s,
        lane_cover,
        &AnyDependencies,
    )
    .unwrap();
    let (after, _) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
    p.activate_pointer(
        None,
        &start(&lane, lane_cover, after.cover),
        &base,
        &s,
        &AnyDependencies,
    )
    .unwrap();
    let expect = Expect {
        ordinal: last + 1,
        seq: 8,
    };
    let (by_head, head_seqs) = recover_from(&store, &lane, BaseSource::Head, expect);
    let (by_pointer, pointer_seqs) = recover_from(&store, &lane, BaseSource::Pointer, expect);
    assert_eq!(head_seqs, pointer_seqs);
    assert_eq!(
        (
            by_head.from_seq,
            by_head.applied,
            by_head.replayed,
            by_head.slots,
            by_head.tail
        ),
        (
            by_pointer.from_seq,
            by_pointer.applied,
            by_pointer.replayed,
            by_pointer.slots,
            by_pointer.tail
        )
    );
    assert_eq!(by_head.base, by_pointer.base);
}

#[test]
fn a_base_published_but_never_named_leaves_the_old_pointer_serving() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    fill(&mut c, 8);
    drop(c);
    let old = published(&store, 4, "ptr-crash-old", 1);
    let p = publisher(&store);
    let s = spec(cut(4), 8, 1);
    p.activate_pointer(None, &start(&lane, 0, 0), &old, &s, &AnyDependencies)
        .unwrap();
    // A newer base's objects are all written, then the writer dies before naming it.
    let _newer = published(&store, 6, "ptr-crash-new", 2);
    let (got, seqs) = recover_from(&store, &lane, BaseSource::Pointer, Expect::default());
    assert_eq!(got.base.as_ref().unwrap().covered, 4);
    assert_eq!(seqs, (4..8).collect::<Vec<_>>());
}

#[test]
fn a_pointer_that_is_not_a_pointer_blocks_recovery() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    fill(&mut c, 3);
    drop(c);
    let key = layout().base_pointer(&INDEX, &SHARD, 1);
    block_on(store.inner_ref().create(
        &ctx(),
        &hsearch_object::Mutation {
            request_id: hsearch_object::RequestId::new(b"garbage").unwrap(),
            key: hsearch_object::ObjectKey::new(key.as_bytes()).unwrap(),
            payload: hsearch_object::Payload::new(b"not a pointer at all").unwrap(),
            metadata: hsearch_object::Metadata::new(b"").unwrap(),
            precondition: hsearch_object::Precondition::Absent,
        },
    ))
    .unwrap();
    let t = Target {
        base: BaseSource::Pointer,
        ..target(&lane, big(), Expect::default())
    };
    let r = recover(
        &publisher(&store),
        &t,
        &AnyDependencies,
        |_: &Record<'_>| Ok(()),
    );
    assert!(matches!(r, Err(NotReady::Base(_))), "{:?}", r.map(|_| ()));
}
