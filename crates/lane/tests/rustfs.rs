//! The lane protocol against a real S3-compatible store (RustFS in CI). Needs `HSEARCH_S3_ENDPOINT`
//! and credentials (`scripts/rustfs.sh up`); without them each test prints SKIPPED unless
//! `HSEARCH_S3_REQUIRED=1`.
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use hsearch_lane::headcas::HeadCasLog;
use hsearch_lane::{
    Error, LaneId, LaneWriter, Placer, block_on, create_lane, read_descriptor, read_log,
    shared_group,
};
use hsearch_s3::{S3Config, S3Store};
use support::*;

static NEXT: AtomicU32 = AtomicU32::new(0);

fn store() -> Option<Arc<S3Store>> {
    let Ok(endpoint) = std::env::var("HSEARCH_S3_ENDPOINT") else {
        assert!(
            std::env::var("HSEARCH_S3_REQUIRED").as_deref() != Ok("1"),
            "HSEARCH_S3_REQUIRED=1 but HSEARCH_S3_ENDPOINT is unset"
        );
        eprintln!(
            "SKIPPED: HSEARCH_S3_ENDPOINT is not set; nothing was checked against a real store"
        );
        return None;
    };
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let bucket = format!("hsl-{nanos}-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let store = S3Store::new(S3Config {
        endpoint,
        bucket,
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
    .unwrap();
    let ctx = hsearch_object::RequestContext::new(
        Duration::from_secs(60),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 1 << 28,
            response_bytes: 1 << 28,
            attempts: 10_000,
        },
    )
    .unwrap();
    store.prepare_bucket(&ctx).expect("prepare bucket");
    Some(Arc::new(store))
}

fn fresh_lane(store: &S3Store, members: &[u32]) -> LaneId {
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

fn open(
    store: &Arc<S3Store>,
    lane: LaneId,
    from: u64,
) -> (LaneWriter<S3Store>, hsearch_lane::OpenReport) {
    block_on(LaneWriter::open(
        Arc::clone(store),
        layout(),
        lane,
        from,
        |_| Ok(()),
    ))
    .unwrap()
}

#[test]
fn a_batch_is_one_put_on_the_wire_and_replay_never_lists() {
    let Some(store) = store() else { return };
    let lane = fresh_lane(&store, &[1, 2]);
    let (mut writer, _) = open(&store, lane, 0);
    let before = store.counts();
    let body = vec![9u8; 4096];
    for n in 0..10u64 {
        block_on(writer.append(&[put(id(1), n, &body), put(id(2), n, &body)])).unwrap();
    }
    let used = store.counts();
    assert_eq!(used.put - before.put, 10, "one PUT per acknowledged batch");
    assert_eq!(
        (
            used.get - before.get,
            used.head - before.head,
            used.list - before.list
        ),
        (0, 0, 0)
    );
    drop(writer);
    let before = store.counts();
    let (_w, report) = open(&store, lane, 0);
    let after = store.counts();
    assert_eq!(
        report.slots_replayed, 11,
        "the first fence and ten data slots"
    );
    assert_eq!(after.list, 0, "no LIST on the open path");
    assert_eq!(after.put - before.put, 1, "one fence PUT");
}

#[test]
fn a_suspended_writer_is_fenced_out_on_a_real_store() {
    let Some(store) = store() else { return };
    let lane = fresh_lane(&store, &[1]);
    let (mut old, _) = open(&store, lane, 0);
    block_on(old.append(&[put(id(1), 1, b"before")])).unwrap();
    let (mut new, _) = open(&store, lane, 0);
    assert_eq!(
        block_on(old.append(&[put(id(1), 2, b"late")])).unwrap_err(),
        Error::Fenced
    );
    block_on(new.append(&[put(id(1), 2, b"after")])).unwrap();
    let mut seqs = Vec::new();
    block_on(read_log(&*store, &layout(), &lane, 0, None, |v| {
        seqs.extend(v.records().map(|r| r.seq));
        Ok(())
    }))
    .unwrap();
    assert_eq!(seqs, vec![1, 2]);
}

#[test]
fn promotion_and_placement_work_on_a_real_store() {
    let Some(store) = store() else { return };
    let mut placer = Placer::with_ceiling(Arc::clone(&store), layout(), 4);
    let mut lane = None;
    for n in 0..6u32 {
        let p = block_on(placer.place(&policy(), id(n))).unwrap();
        assert_eq!(p.group, u64::from(n / 4));
        if n == 0 {
            lane = Some(p.lane);
        }
    }
    let lane = lane.unwrap();
    let (mut writer, _) = open(&store, lane, 0);
    block_on(writer.append(&[put(id(0), 1, b"x"), put(id(1), 1, b"y")])).unwrap();
    let done = block_on(writer.promote(id(0), 1)).unwrap();
    assert_eq!(
        block_on(read_descriptor(&*store, &layout(), &lane))
            .unwrap()
            .0
            .members,
        vec![id(1), id(2), id(3)]
    );
    assert_eq!(
        block_on(read_descriptor(&*store, &layout(), &done.target))
            .unwrap()
            .0
            .members,
        vec![id(0)]
    );
    assert_eq!(
        block_on(writer.append(&[put(id(0), 2, b"z")])).unwrap_err(),
        Error::Sealed(id(0))
    );
    block_on(writer.append(&[put(id(1), 2, b"mate")])).unwrap();
    let (mut target, _) = open(&store, done.target, 0);
    block_on(target.append(&[put(id(0), 2, b"now here")])).unwrap();
    assert_eq!(store.counts().list, 0);
}

#[test]
fn head_cas_comparison_costs_two_puts_and_fences_on_a_real_store() {
    let Some(store) = store() else { return };
    let lane = LaneId::new(id(77));
    let mut old = block_on(HeadCasLog::open(Arc::clone(&store), layout(), lane)).unwrap();
    let before = store.counts();
    block_on(old.append(b"a")).unwrap();
    assert_eq!(
        store.counts().put - before.put,
        2,
        "payload PUT plus head CAS"
    );
    let mut new = block_on(HeadCasLog::open(Arc::clone(&store), layout(), lane)).unwrap();
    assert_eq!(block_on(old.append(b"stale")).unwrap_err(), Error::Fenced);
    block_on(new.append(b"b")).unwrap();
    let mut got = Vec::new();
    block_on(HeadCasLog::read_all(&*store, &layout(), &lane, |_, p| {
        got.push(p.to_vec())
    }))
    .unwrap();
    assert_eq!(got, vec![b"a".to_vec(), b"b".to_vec()]);
}

#[test]
fn reclaim_keeps_fences_and_refuses_an_open_below_the_cover_on_a_real_store() {
    let Some(store) = store() else { return };
    let lane = fresh_lane(&store, &[1]);
    let (mut old, _) = open(&store, lane, 0);
    for n in 0..3u64 {
        block_on(old.append(&[put(id(1), n, b"x")])).unwrap();
    }
    let (mut new, _) = open(&store, lane, 0); // fence at 4
    block_on(new.append(&[put(id(1), 9, b"y")])).unwrap(); // 5
    let done = block_on(new.reclaim_below(5)).unwrap();
    assert_eq!((done.removed, done.fences_kept), (3, 2));
    assert_eq!(
        block_on(old.append(&[put(id(1), 7, b"late")])).unwrap_err(),
        Error::Fenced
    );
    for from in [0, 1, 2] {
        let err = block_on(LaneWriter::open(
            Arc::clone(&store),
            layout(),
            lane,
            from,
            |_| Ok(()),
        ))
        .err()
        .unwrap();
        assert_eq!(err, Error::BadCover, "from {from}");
    }
    let (_w, report) = open(&store, lane, 5);
    assert_eq!(report.slots_replayed, 1);
}
