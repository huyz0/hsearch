//! Spill, external payloads and sealed successors against a real S3-compatible store (RustFS in
//! CI): ranged payload reads, a promoted index's two lanes, and a scratch-backed replay. Needs
//! `HSEARCH_S3_ENDPOINT`, `HSEARCH_S3_ACCESS_KEY` and `HSEARCH_S3_SECRET_KEY`; without them each
//! test prints SKIPPED and passes, unless `HSEARCH_S3_REQUIRED=1` (CI).
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use hsearch_format::{Durability, Id, KeyIdentity, PayloadRef, PolicyTuple, RecordOp, RecordSpec};
use hsearch_lane::{LaneId, LaneWriter, block_on, create_head, create_lane, shared_group};
use hsearch_object::{
    Budget, Cancellation, Metadata, Mutation as Put, ObjectBackend, ObjectKey, Payload,
    Precondition, RequestContext, RequestId,
};
use hsearch_s3::{S3Config, S3Store};
use hsearch_store::publish::{AnyDependencies, Publisher};
use hsearch_store::recover::{Expect, Record, ReplayLimits, Spill, Target, recover_with};
use sha2::{Digest, Sha256};
use support::{INDEX, SHARD, layout};

const B: Id = [1; 16];
static NEXT: AtomicU32 = AtomicU32::new(0);

fn ctx() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: 32 << 20,
            response_bytes: 8 << 20,
            attempts: 8,
        },
    )
    .unwrap()
}

/// A store on a fresh versioned bucket, or `None` when no endpoint is configured.
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
    let store = S3Store::new(S3Config {
        endpoint,
        bucket: format!("hs-store-{nanos}-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
    .unwrap();
    store.prepare_bucket(&ctx()).expect("prepare bucket");
    Some(Arc::new(store))
}

fn policy() -> PolicyTuple {
    PolicyTuple {
        key: KeyIdentity::DeploymentDefault,
        residency: "region-a".into(),
        durability: Durability::Object,
        format_version: 1,
    }
}

fn sha(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

fn doc(i: u64) -> Vec<u8> {
    format!("doc-{i}").into_bytes()
}

fn inline(index: Id, seq: u64, body: &[u8]) -> RecordSpec<'_> {
    RecordSpec {
        index,
        seq,
        op: RecordOp::Put,
        payload: PayloadRef::Inline(body),
    }
}

fn lane_of(store: &Arc<S3Store>) -> (LaneId, LaneWriter<S3Store>) {
    let (domain, group) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    block_on(create_lane(
        &**store,
        &layout(),
        &lane,
        &policy(),
        group,
        1000,
        &[B, INDEX],
    ))
    .unwrap();
    block_on(create_head(&**store, &layout(), &lane, Durability::Object)).unwrap();
    (lane, writer(store, lane))
}

fn writer(store: &Arc<S3Store>, lane: LaneId) -> LaneWriter<S3Store> {
    block_on(LaneWriter::open_with_head(
        Arc::clone(store),
        layout(),
        lane,
        |_| Ok(()),
    ))
    .unwrap()
    .0
}

fn put_payload(store: &Arc<S3Store>, object: &Id, bytes: &[u8]) {
    let key = layout().payload(&INDEX, object);
    let m = Put {
        request_id: RequestId::new(&sha(key.as_bytes())).unwrap(),
        key: ObjectKey::new(key.as_bytes()).unwrap(),
        payload: Payload::new(bytes).unwrap(),
        metadata: Metadata::new(b"").unwrap(),
        precondition: Precondition::Absent,
    };
    block_on(store.create(&ctx(), &m)).unwrap();
}

fn recovered(
    store: &Arc<S3Store>,
    lane: &LaneId,
    records: u64,
    spill: &Spill,
) -> (hsearch_store::recover::Recovered, Vec<(u64, Vec<u8>)>) {
    let p = Publisher::new(
        Arc::clone(store),
        layout(),
        5 << 20,
        Duration::from_secs(60),
    );
    let target = Target {
        lane,
        index: INDEX,
        shard: SHARD,
        incarnation: 1,
        limits: ReplayLimits {
            slots: 1 << 20,
            records,
            bytes: 1 << 30,
        },
        expect: Expect::default(),
        base: hsearch_store::pointer::BaseSource::Head,
    };
    let mut got = Vec::new();
    let r = recover_with(&p, &target, &AnyDependencies, spill, |r: &Record<'_>| {
        got.push((r.seq, r.body.to_vec()));
        Ok(())
    })
    .unwrap();
    (r, got)
}

#[test]
fn external_payloads_are_read_in_ranges_and_verified_on_a_real_store() {
    let Some(store) = store() else { return };
    let (lane, mut w) = lane_of(&store);
    // 12 MiB: one 8 MiB range and a 4 MiB one.
    let large: Vec<u8> = (0..(12u32 << 20)).map(|i| (i % 251) as u8).collect();
    put_payload(&store, &[2; 16], &large);
    let ext = RecordSpec {
        index: INDEX,
        seq: 1,
        op: RecordOp::Put,
        payload: PayloadRef::External {
            object: [2; 16],
            length: large.len() as u64,
            digest: sha(&large),
        },
    };
    block_on(w.append(&[inline(INDEX, 0, &doc(0)), ext, inline(INDEX, 2, &doc(2))])).unwrap();
    drop(w);
    let (r, got) = recovered(&store, &lane, 100, &Spill::Refuse);
    assert_eq!(r.applied, 3);
    assert_eq!(got, vec![(0, doc(0)), (1, large), (2, doc(2))]);
}

#[test]
fn a_promoted_index_is_followed_and_a_long_tail_spills_on_a_real_store() {
    let Some(store) = store() else { return };
    let (lane, mut w) = lane_of(&store);
    for i in 0..40 {
        block_on(w.append(&[inline(INDEX, i, &doc(i))])).unwrap();
    }
    let moved = block_on(w.promote(INDEX, 39)).unwrap();
    let mut t = writer(&store, moved.target);
    for i in 40..120 {
        block_on(t.append(&[inline(INDEX, i, &doc(i))])).unwrap();
    }
    drop((w, t));
    let dir = std::env::temp_dir().join(format!("hsearch-spill-s3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let spill = Spill::Scratch {
        dir: dir.clone(),
        ceiling: ReplayLimits {
            slots: 1 << 20,
            records: 1 << 20,
            bytes: 1 << 30,
        },
    };
    let (r, got) = recovered(&store, &lane, 10, &spill);
    assert_eq!((r.applied, r.followed, r.spilled), (120, 1, 110));
    assert_eq!(got, (0..120).map(|i| (i, doc(i))).collect::<Vec<_>>());
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}
