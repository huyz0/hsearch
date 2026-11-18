//! Helpers for the spill, external-payload and sealed-successor recovery tests: a store that counts
//! the reads recovery makes, payload objects, a raw lane writer, and a model ledger.
#![allow(dead_code)]
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hsearch_format::{Id, PayloadRef, RecordOp, RecordSpec};
use hsearch_lane::{LaneId, LaneWriter, block_on};
use hsearch_object::{
    Budget, ByteRange, Cancellation, LargeObjectWriter, Metadata, MetadataStore, Mutation,
    ObjectBackend, ObjectInfo, ObjectKey, Operation, Payload, Precondition, RequestContext,
    RequestId, VersionToken, WriteReceipt,
};
use hsearch_store::publish::{AnyDependencies, Publisher};
use hsearch_store::recover::recover_with;
use hsearch_store::recover::{Expect, NotReady, Record, Recovered, ReplayLimits, Spill};
use sha2::{Digest, Sha256};

use crate::support::*;

/// Counts the HEADs and ranged reads of payload objects (not slots, heads or manifests) that pass
/// through, and the bytes the reads return.
pub struct Tally {
    pub inner: Arc<Crash>,
    pub heads: AtomicU64,
    pub reads: AtomicU64,
    pub read_bytes: AtomicU64,
}

fn is_payload(k: &ObjectKey) -> bool {
    k.as_bytes().windows(10).any(|w| w == b"/payloads/")
}

impl Tally {
    pub fn counts(&self) -> (u64, u64) {
        (
            self.heads.load(Ordering::SeqCst),
            self.reads.load(Ordering::SeqCst),
        )
    }
}

impl ObjectBackend for Tally {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        if is_payload(k) {
            self.heads.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        self.inner.create(c, r)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        if is_payload(k) {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.read_bytes.fetch_add(r.length(), Ordering::SeqCst);
        }
        self.inner.read_range(c, k, v, r)
    }
}

impl MetadataStore for Tally {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.inner.compare_exchange(c, r)
    }
    fn remove_version<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
    ) -> Operation<'a, ()> {
        self.inner.remove_version(c, k, v)
    }
    fn resolve<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.inner.resolve(c, r)
    }
}

impl LargeObjectWriter for Tally {
    type Upload = <Crash as LargeObjectWriter>::Upload;
    fn begin_upload<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
    ) -> Operation<'a, Self::Upload> {
        self.inner.begin_upload(c, k)
    }
    fn put_part<'a>(
        &'a self,
        c: &'a RequestContext,
        u: &'a mut Self::Upload,
        b: &'a [u8],
    ) -> Operation<'a, ()> {
        self.inner.put_part(c, u, b)
    }
    fn complete_upload<'a>(
        &'a self,
        c: &'a RequestContext,
        u: &'a Self::Upload,
    ) -> Operation<'a, (VersionToken, u64)> {
        self.inner.complete_upload(c, u)
    }
    fn abort_upload<'a>(&'a self, c: &'a RequestContext, u: &'a Self::Upload) -> Operation<'a, ()> {
        self.inner.abort_upload(c, u)
    }
}

/// A publisher reading through a counting wrapper of `store`.
pub fn tally(store: &Arc<Crash>) -> (Publisher<Tally>, Arc<Tally>) {
    let t = Arc::new(Tally {
        inner: Arc::clone(store),
        heads: AtomicU64::new(0),
        reads: AtomicU64::new(0),
        read_bytes: AtomicU64::new(0),
    });
    let p = Publisher::new(Arc::clone(&t), layout(), 5 << 20, Duration::from_secs(30));
    (p, t)
}

pub fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Write `bytes` as the payload object `object` of `index`.
pub fn put_payload(store: &Arc<Crash>, index: &Id, object: &Id, bytes: &[u8]) {
    let key = layout().payload(index, object);
    let ctx = RequestContext::new(
        Duration::from_secs(30),
        Cancellation::default(),
        Budget {
            request_bytes: 32 << 20,
            response_bytes: 1 << 20,
            attempts: 8,
        },
    )
    .unwrap();
    let m = Mutation {
        request_id: RequestId::new(&sha(key.as_bytes())).unwrap(),
        key: ObjectKey::new(key.as_bytes()).unwrap(),
        payload: Payload::new(bytes).unwrap(),
        metadata: Metadata::new(b"").unwrap(),
        precondition: Precondition::Absent,
    };
    block_on(store.create(&ctx, &m)).unwrap();
}

/// The single writer of `lane`, appending records exactly as given.
pub fn raw_writer(store: &Arc<Crash>, lane: LaneId) -> LaneWriter<Crash> {
    block_on(LaneWriter::open_with_head(
        Arc::clone(store),
        layout(),
        lane,
        |_| Ok(()),
    ))
    .unwrap()
    .0
}

pub fn inline<'a>(index: Id, seq: u64, body: &'a [u8]) -> RecordSpec<'a> {
    RecordSpec {
        index,
        seq,
        op: RecordOp::Put,
        payload: PayloadRef::Inline(body),
    }
}

pub fn external(index: Id, seq: u64, object: Id, body: &[u8]) -> RecordSpec<'static> {
    RecordSpec {
        index,
        seq,
        op: RecordOp::Put,
        payload: PayloadRef::External {
            object,
            length: body.len() as u64,
            digest: sha(body),
        },
    }
}

/// The model body of sequence `i`.
pub fn doc(i: u64) -> Vec<u8> {
    format!("doc-{i}").into_bytes()
}

/// Append inline records `from..to` of `index`, fifty to a slot.
pub fn append_docs(w: &mut LaneWriter<Crash>, index: Id, from: u64, to: u64) {
    let seqs: Vec<u64> = (from..to).collect();
    for group in seqs.chunks(50) {
        let bodies: Vec<Vec<u8>> = group.iter().map(|i| doc(*i)).collect();
        let specs: Vec<RecordSpec<'_>> = group
            .iter()
            .zip(&bodies)
            .map(|(i, b)| inline(index, *i, b))
            .collect();
        block_on(w.append(&specs)).unwrap();
    }
}

pub fn model(n: u64) -> Vec<(u64, Vec<u8>)> {
    (0..n).map(|i| (i, doc(i))).collect()
}

pub fn limits(records: u64) -> ReplayLimits {
    ReplayLimits {
        slots: 1_000_000,
        records,
        bytes: 1 << 30,
    }
}

/// What a recovery returned and the (sequence, body) pairs it applied.
pub type Outcome = (Result<Recovered, NotReady>, Vec<(u64, Vec<u8>)>);

/// A way to prepare the store before a case runs.
pub type Prepare = Box<dyn Fn(&Arc<Crash>)>;

/// Recover INDEX from `lane` and return what was applied, in order.
pub fn run_with<S>(
    p: &Publisher<S>,
    lane: &LaneId,
    limits: ReplayLimits,
    expect: Expect,
    spill: &Spill,
) -> Outcome
where
    S: MetadataStore + LargeObjectWriter + 'static,
{
    let mut got = Vec::new();
    let result = recover_with(
        p,
        &target(lane, limits, expect),
        &AnyDependencies,
        spill,
        |r: &Record<'_>| {
            got.push((r.seq, r.body.to_vec()));
            Ok(())
        },
    );
    (result, got)
}

/// A fresh scratch directory.
pub fn scratch_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("hsearch-spill-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write one data slot holding exactly `records` at `ordinal`, bypassing the writer's own checks.
pub fn raw_slot(store: &Arc<Crash>, lane: &LaneId, ordinal: u64, records: &[RecordSpec<'_>]) {
    use hsearch_format::{SlotBody, SlotPosition, encode_slot};
    let mut bytes = Vec::new();
    encode_slot(
        &mut bytes,
        &SlotPosition {
            domain: lane.domain,
            activation: lane.activation,
            lane: lane.lane,
            ordinal,
        },
        token(9),
        &SlotBody::Data(records),
    )
    .unwrap();
    let key = layout().lane_slot(
        &lane.domain,
        lane.incarnation,
        lane.activation,
        lane.lane,
        ordinal,
    );
    let ctx = RequestContext::new(
        Duration::from_secs(30),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 8,
        },
    )
    .unwrap();
    let m = Mutation {
        request_id: RequestId::new(&sha(key.as_bytes())).unwrap(),
        key: ObjectKey::new(key.as_bytes()).unwrap(),
        payload: Payload::new(&bytes).unwrap(),
        metadata: Metadata::new(b"").unwrap(),
        precondition: Precondition::Absent,
    };
    block_on(store.create(&ctx, &m)).unwrap();
}

pub fn seal(index: Id, seq: u64) -> RecordSpec<'static> {
    RecordSpec {
        index,
        seq,
        op: RecordOp::Seal,
        payload: PayloadRef::None,
    }
}
