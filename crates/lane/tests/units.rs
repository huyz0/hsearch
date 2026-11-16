//! Pure and near-pure pieces: the batcher, the hot rule and the head-CAS comparison log.
mod support;

use std::sync::Arc;
use std::time::Duration;

use hsearch_format::RecordOp;
use hsearch_lane::batch::{BatchPolicy, Batcher, OwnedPayload, OwnedRecord};
use hsearch_lane::headcas::HeadCasLog;
use hsearch_lane::hot::{HotRule, HotTracker};
use hsearch_lane::{Error, LaneId, block_on};
use support::*;

fn rec(n: u32, bytes: usize) -> OwnedRecord {
    OwnedRecord {
        index: id(n),
        seq: 1,
        op: RecordOp::Put,
        payload: OwnedPayload::Inline(vec![1; bytes]),
    }
}

const MS: fn(u64) -> Duration = Duration::from_millis;

#[test]
fn an_idle_batcher_has_no_deadline_and_a_busy_one_closes_after_the_wait() {
    let mut b = Batcher::new(BatchPolicy {
        max_wait: MS(250),
        ..BatchPolicy::default()
    })
    .unwrap();
    assert_eq!(b.deadline(), None, "nothing pending, nothing to wake for");
    assert!(b.poll(MS(10_000)).is_none());
    assert!(b.flush().is_none());
    assert!(b.push(rec(1, 10), MS(1000)).unwrap().is_empty());
    assert_eq!(
        b.deadline(),
        Some(MS(1250)),
        "the wait runs from the first record"
    );
    assert!(b.push(rec(2, 10), MS(1200)).unwrap().is_empty());
    assert!(b.poll(MS(1249)).is_none());
    let batch = b.poll(MS(1250)).unwrap();
    assert_eq!((batch.records.len(), batch.opened), (2, MS(1000)));
    assert_eq!(b.deadline(), None);
    assert_eq!((b.pending_records(), b.pending_bytes()), (0, 0));
}

#[test]
fn batches_close_at_the_byte_target_the_record_cap_and_a_big_record_stands_alone() {
    let policy = BatchPolicy {
        max_records: 3,
        max_bytes: 1000,
        max_wait: MS(250),
    };
    let mut b = Batcher::new(policy).unwrap();
    // The record cap closes the third push itself.
    assert!(b.push(rec(1, 10), MS(0)).unwrap().is_empty());
    assert!(b.push(rec(2, 10), MS(0)).unwrap().is_empty());
    let closed = b.push(rec(3, 10), MS(0)).unwrap();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].records.len(), 3);
    // A record that would overflow the bytes closes the pending batch first.
    assert!(b.push(rec(4, 400), MS(0)).unwrap().is_empty());
    let closed = b.push(rec(5, 700), MS(1)).unwrap();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].records[0].index, id(4));
    // A record over the target is its own batch and flushes what it displaced.
    let closed = b.push(rec(6, 2000), MS(2)).unwrap();
    assert_eq!(
        closed.len(),
        2,
        "the pending record, then the big one alone"
    );
    assert_eq!(closed[1].records.len(), 1);
    assert!(closed.iter().all(|c| c.bytes > 0));
    assert_eq!(b.pending_records(), 0);
    // The cap keeps memory bounded: nothing is ever held above max_bytes plus one record.
    for n in 0..100 {
        let _ = b.push(rec(n, 300), MS(3)).unwrap();
        assert!(b.pending_bytes() <= 1000);
    }
}

#[test]
fn the_batcher_refuses_bad_policies_and_bad_inline_sizes() {
    let bad = |p: BatchPolicy| Batcher::new(p).is_err();
    assert!(bad(BatchPolicy {
        max_records: 0,
        ..BatchPolicy::default()
    }));
    assert!(bad(BatchPolicy {
        max_records: 4097,
        ..BatchPolicy::default()
    }));
    assert!(bad(BatchPolicy {
        max_bytes: 0,
        ..BatchPolicy::default()
    }));
    assert!(bad(BatchPolicy {
        max_bytes: usize::MAX,
        ..BatchPolicy::default()
    }));
    let mut b = Batcher::new(BatchPolicy::default()).unwrap();
    assert!(b.push(rec(1, 0), MS(0)).is_err());
    assert!(b.push(rec(1, (1 << 20) + 1), MS(0)).is_err());
    assert_eq!(b.pending_records(), 0, "a refused record is not queued");
}

#[test]
fn a_batch_hands_its_records_to_the_slot_encoder_unchanged() {
    let mut b = Batcher::new(BatchPolicy::default()).unwrap();
    b.push(rec(1, 5), MS(0)).unwrap();
    b.push(
        OwnedRecord {
            index: id(2),
            seq: 9,
            op: RecordOp::Delete,
            payload: OwnedPayload::None,
        },
        MS(0),
    )
    .unwrap();
    let batch = b.flush().unwrap();
    let specs = batch.specs();
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[1].seq, 9);
    let total: usize = batch.records.iter().map(OwnedRecord::encoded_len).sum();
    assert_eq!(batch.bytes, total);
}

fn rule() -> HotRule {
    HotRule {
        window: Duration::from_secs(10),
        share_num: 1,
        share_den: 4,
        floor_records: 100,
    }
}

#[test]
fn a_quarter_of_the_bytes_on_a_busy_lane_is_hot_and_just_under_is_not() {
    let mut t = HotTracker::new(rule(), 4096);
    // 100 records of 100 bytes: index 1 takes exactly 25 of them = a quarter.
    for n in 0..100u32 {
        let index = if n < 25 { 1 } else { 100 + n };
        assert!(t.record(id(index), 100, Duration::from_secs(1)).is_none());
    }
    let report = t.record(id(7), 100, Duration::from_secs(12)).unwrap();
    assert_eq!(report.records, 100);
    assert_eq!(report.hot.len(), 1);
    assert_eq!(report.hot[0].index, id(1));
    assert!((report.hottest_byte_share - 0.25).abs() < 1e-9);
    // 24 of 100 is under the quarter.
    let mut t = HotTracker::new(rule(), 4096);
    for n in 0..100u32 {
        let index = if n < 24 { 1 } else { 100 + n };
        t.record(id(index), 100, Duration::from_secs(1));
    }
    assert!(t.finish().hot.is_empty());
}

#[test]
fn below_the_volume_floor_nothing_is_hot_however_lopsided() {
    let mut t = HotTracker::new(rule(), 4096);
    t.record(id(1), 10_000, Duration::from_secs(0));
    t.record(id(1), 10_000, Duration::from_secs(1));
    let report = t.finish();
    assert!(report.hot.is_empty());
    assert_eq!(
        report.hottest_byte_share, 0.0,
        "two records are a 100% share of nothing"
    );
}

#[test]
fn share_is_by_bytes_not_records_and_state_is_bounded() {
    let mut t = HotTracker::new(rule(), 4);
    // Index 1: 10 records of 1000 bytes; ninety others of 10 bytes.
    for _ in 0..10 {
        t.record(id(1), 1000, Duration::from_secs(0));
    }
    for n in 0..90u32 {
        t.record(id(100 + n), 10, Duration::from_secs(0));
    }
    let report = t.finish();
    assert_eq!(
        report.hot.len(),
        1,
        "10% of the records but 92% of the bytes"
    );
    assert_eq!(report.hot[0].index, id(1));
    assert!(
        report.untracked > 0,
        "indexes beyond the bound are counted, not stored"
    );
}

fn head_lane() -> LaneId {
    LaneId::new(id(42))
}

#[test]
fn head_cas_commits_with_two_puts_and_reads_back_in_order() {
    let store = counted();
    let mut log = block_on(HeadCasLog::open(Arc::clone(&store), layout(), head_lane())).unwrap();
    let before = store.requests();
    assert_eq!(block_on(log.append(b"one")).unwrap(), 0);
    assert_eq!(block_on(log.append(b"two")).unwrap(), 1);
    let used = store.requests().since(&before);
    assert_eq!(
        (used.create, used.cas),
        (2, 2),
        "a payload create and a head CAS per batch"
    );
    let mut got = Vec::new();
    let n = block_on(HeadCasLog::read_all(
        &*store,
        &layout(),
        &head_lane(),
        |o, p| got.push((o, p.to_vec())),
    ))
    .unwrap();
    assert_eq!(
        (n, got),
        (2, vec![(0, b"one".to_vec()), (1, b"two".to_vec())])
    );
}

#[test]
fn head_cas_fences_a_stale_writer_and_never_reads_its_orphan() {
    let store = counted();
    let mut old = block_on(HeadCasLog::open(Arc::clone(&store), layout(), head_lane())).unwrap();
    block_on(old.append(b"a")).unwrap();
    let mut new = block_on(HeadCasLog::open(Arc::clone(&store), layout(), head_lane())).unwrap();
    assert_eq!(
        block_on(old.append(b"stale")).unwrap_err(),
        Error::Fenced,
        "its head CAS lost"
    );
    assert!(old.is_poisoned());
    assert_eq!(block_on(new.append(b"b")).unwrap(), 1);
    let mut got = Vec::new();
    block_on(HeadCasLog::read_all(
        &*store,
        &layout(),
        &head_lane(),
        |_, p| got.push(p.to_vec()),
    ))
    .unwrap();
    assert_eq!(
        got,
        vec![b"a".to_vec(), b"b".to_vec()],
        "the orphan 'stale' payload is unreferenced"
    );
}
