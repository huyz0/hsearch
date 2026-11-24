//! Shared helpers for the store tests: a fault-injecting store and engine commits on disk.
#![allow(dead_code, unused_imports)]
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use hsearch_commit::{AnyDependencies as AnyDeps, Committer, Outcome, Receipt, Request};
use hsearch_engine::{Kind, LocalFiles, Mutation, Schema, Value, with_local_engine};
use hsearch_format::{CommitToken, Durability, Id, Layout, StructuralHead};
use hsearch_format::{KeyIdentity, PolicyTuple};
use hsearch_index::barrier::{Cut, Replay};
use hsearch_lane::batch::BatchPolicy;
use hsearch_lane::{LaneId, block_on, create_head, read_head};
use hsearch_lane::{LaneWriter, create_lane, shared_group};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{Budget, Cancellation};
use hsearch_object::{
    ByteRange, Error as StoreError, LargeObjectWriter, MetadataStore, Mutation as Put,
    ObjectBackend, ObjectInfo, ObjectKey, Operation, Payload, RequestContext, VersionToken,
    WriteReceipt,
};
use hsearch_store::publish::{AnyDependencies, BaseSpec, Dependencies, Error, HeadRead, Publisher};
use hsearch_store::recover::{Expect, ReplayLimits, Target};
use sha2::{Digest, Sha256};

pub const INDEX: Id = [7; 16];
pub const SHARD: Id = [8; 16];

/// A store that refuses every write (create, compare-exchange, upload step) once its allowance is
/// spent, and counts the writes it let through.
pub struct Crash {
    pub inner: MemoryStore,
    pub allow: AtomicI64,
    pub writes: AtomicUsize,
    /// The write (counted from 0) whose reply is lost after it commits; negative for none.
    pub lose: AtomicI64,
    pub fail_resolve: std::sync::atomic::AtomicBool,
}

impl Crash {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(MemoryLimits {
                objects: 4096,
                versions: 16384,
                version_bytes: 64 << 20,
                identities: 16384,
                identity_bytes: 64 << 20,
            })
            .unwrap(),
            allow: AtomicI64::new(i64::MAX),
            writes: AtomicUsize::new(0),
            lose: AtomicI64::new(-1),
            fail_resolve: std::sync::atomic::AtomicBool::new(false),
        })
    }
    pub fn spend(&self) -> bool {
        if self.allow.fetch_sub(1, Ordering::SeqCst) > 0 {
            self.writes.fetch_add(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
    /// True when this write's reply is to be lost after it commits.
    pub fn lost(&self) -> bool {
        self.writes.load(Ordering::SeqCst) as i64 - 1 == self.lose.load(Ordering::SeqCst)
    }
    pub fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
}

impl ObjectBackend for Crash {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Put) -> Operation<'a, WriteReceipt> {
        if !self.spend() {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        if self.lost() {
            return Box::pin(async move {
                self.inner.create(c, r).await?;
                Err(StoreError::UnknownOutcome)
            });
        }
        self.inner.create(c, r)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        self.inner.read_range(c, k, v, r)
    }
}

impl MetadataStore for Crash {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Put,
    ) -> Operation<'a, WriteReceipt> {
        if !self.spend() {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        if self.lost() {
            return Box::pin(async move {
                self.inner.compare_exchange(c, r).await?;
                Err(StoreError::UnknownOutcome)
            });
        }
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
    fn resolve<'a>(&'a self, c: &'a RequestContext, r: &'a Put) -> Operation<'a, WriteReceipt> {
        if self.fail_resolve.load(Ordering::SeqCst) {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        self.inner.resolve(c, r)
    }
}

impl LargeObjectWriter for Crash {
    type Upload = <MemoryStore as LargeObjectWriter>::Upload;
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
        if !self.spend() {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        self.inner.put_part(c, u, b)
    }
    fn complete_upload<'a>(
        &'a self,
        c: &'a RequestContext,
        u: &'a Self::Upload,
    ) -> Operation<'a, (VersionToken, u64)> {
        if !self.spend() {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        self.inner.complete_upload(c, u)
    }
    fn abort_upload<'a>(&'a self, c: &'a RequestContext, u: &'a Self::Upload) -> Operation<'a, ()> {
        self.inner.abort_upload(c, u)
    }
}

pub fn layout() -> Layout {
    Layout::new("test").unwrap()
}

pub fn publisher(store: &Arc<Crash>) -> Publisher<Crash> {
    Publisher::new(
        Arc::clone(store),
        layout(),
        5 << 20,
        Duration::from_secs(30),
    )
}

pub fn token(n: u8) -> CommitToken {
    CommitToken::new([n; 16]).unwrap()
}

/// A cut covering the first `n` slots, as a replay barrier produces it.
pub fn cut(n: u64) -> Cut {
    let mut replay = Replay::new(0);
    for seq in 0..n {
        replay.begin(seq).unwrap();
        replay.done(seq).unwrap();
    }
    replay.cut().unwrap()
}

/// Commit a few documents to a scratch engine and run `f` while its native files are on disk.
pub fn with_commit<R>(
    label: &str,
    f: impl FnOnce(&LocalFiles, &std::path::Path, &[String]) -> R,
) -> R {
    let dir = std::env::temp_dir().join(format!("hsearch-store-{label}-{}", std::process::id()));
    let schema = Schema::new(1, &[("tag", Kind::Keyword)]).unwrap();
    let fields = [("tag", Value::String("x"))];
    let result = with_local_engine(&dir, schema.clone(), |engine, local| {
        let puts: Vec<Mutation<'_>> = (0..5)
            .map(|i| Mutation::Put {
                id: ["a", "b", "c", "d", "e"][i],
                version: 1,
                fields: &fields,
            })
            .collect();
        engine.apply(schema.prepare("r", &puts).unwrap()).unwrap();
        engine.commit().unwrap();
        let names = local.names().unwrap();
        f(local, &dir, &names)
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

pub fn spec<'a>(cut: Cut, committed: u64, attempt: u8) -> BaseSpec<'a> {
    BaseSpec {
        index: INDEX,
        shard: SHARD,
        incarnation: 1,
        engine_format: "lucene-test",
        mapping: [5; 32],
        crypto_envelope: [6; 16],
        cut,
        committed,
        previous: None,
        attempt: token(attempt),
    }
}

pub fn lane() -> LaneId {
    LaneId::new([9; 16])
}

pub fn head_of(store: &Arc<Crash>) -> (StructuralHead, VersionToken) {
    block_on(read_head(store.inner_ref(), &layout(), &lane())).unwrap()
}

impl Crash {
    pub fn inner_ref(&self) -> &MemoryStore {
        &self.inner
    }
}

pub fn fresh_head(store: &Arc<Crash>) {
    block_on(create_head(
        store.inner_ref(),
        &layout(),
        &lane(),
        Durability::Object,
    ))
    .unwrap();
}

pub fn read_all(store: &Arc<Crash>, key: &str) -> Vec<u8> {
    let key = ObjectKey::new(key.as_bytes()).unwrap();
    let ctx = || {
        RequestContext::new(
            Duration::from_secs(30),
            hsearch_object::Cancellation::default(),
            hsearch_object::Budget {
                request_bytes: 4096,
                response_bytes: 64 << 20,
                attempts: 8,
            },
        )
        .unwrap()
    };
    let info = block_on(store.inner.head(&ctx(), &key)).unwrap();
    block_on(store.inner.read_range(
        &ctx(),
        &key,
        &info.version,
        ByteRange::new(0, info.length).unwrap(),
    ))
    .unwrap()
    .as_bytes()
    .to_vec()
}

pub fn at<'a>(
    lane: &'a LaneId,
    head: &'a StructuralHead,
    version: &'a VersionToken,
) -> HeadRead<'a> {
    HeadRead {
        lane,
        head,
        version,
    }
}

pub const B: Id = [1; 16];

pub fn policy() -> PolicyTuple {
    PolicyTuple {
        key: KeyIdentity::DeploymentDefault,
        residency: "region-a".into(),
        durability: Durability::Object,
        format_version: 1,
    }
}

pub fn big() -> ReplayLimits {
    ReplayLimits {
        slots: 10_000,
        records: 10_000,
        bytes: 64 << 20,
    }
}

pub fn target(lane: &LaneId, limits: ReplayLimits, expect: Expect) -> Target<'_> {
    Target {
        lane,
        index: INDEX,
        shard: SHARD,
        incarnation: 1,
        limits,
        expect,
        base: hsearch_store::pointer::BaseSource::Head,
    }
}

pub fn ctx() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(30),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 8,
        },
    )
    .unwrap()
}

/// A lane of two indexes with its head, and the committer that writes it one record to a slot.
pub fn open_lane(store: &Arc<Crash>) -> (LaneId, Committer<Crash, AnyDeps>) {
    let (domain, group) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    block_on(create_lane(
        store.inner_ref(),
        &layout(),
        &lane,
        &policy(),
        group,
        1000,
        &[B, INDEX],
    ))
    .unwrap();
    block_on(create_head(
        store.inner_ref(),
        &layout(),
        &lane,
        Durability::Object,
    ))
    .unwrap();
    (lane, writer(store, lane))
}

pub fn writer(store: &Arc<Crash>, lane: LaneId) -> Committer<Crash, AnyDeps> {
    let (w, _) = block_on(LaneWriter::open_with_head(
        Arc::clone(store),
        layout(),
        lane,
        |_| Ok(()),
    ))
    .unwrap();
    let policy = BatchPolicy {
        max_records: 1,
        max_bytes: 4 << 20,
        max_wait: Duration::from_millis(1),
    };
    Committer::new(w, AnyDeps, policy).unwrap()
}

pub fn req(index: Id, seq: u64, body: &str) -> Request {
    Request {
        index,
        seq,
        body: body.as_bytes().to_vec(),
        delete: false,
        deps: hsearch_commit::Dependencies {
            schema: [1; 32],
            crypto: [1; 32],
            resolved: [1; 32],
        },
    }
}

pub fn commit(c: &mut Committer<Crash, AnyDeps>, r: Request) -> Receipt {
    match block_on(c.commit(vec![r])).remove(0) {
        Outcome::Committed(receipt) => receipt,
        other => panic!("not committed: {other:?}"),
    }
}

pub fn del(index: Id, seq: u64) -> Request {
    Request {
        delete: true,
        body: Vec::new(),
        ..req(index, seq, "")
    }
}
