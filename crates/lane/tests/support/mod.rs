//! Shared helpers for the lane tests: stores, ids, policies and a fault-injecting wrapper.
#![allow(dead_code)]
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use hsearch_format::{
    Durability, Id, KeyIdentity, Layout, PayloadRef, PolicyTuple, RecordOp, RecordSpec,
};
use hsearch_lane::counting::CountingStore;
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    ByteRange, Error as StoreError, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey,
    Operation, Payload, RequestContext, VersionToken, WriteReceipt,
};

pub fn id(n: u32) -> Id {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&n.to_be_bytes());
    out[15] = 0xA5;
    out
}

pub fn layout() -> Layout {
    Layout::new("test").unwrap()
}

pub fn policy() -> PolicyTuple {
    PolicyTuple {
        key: KeyIdentity::DeploymentDefault,
        residency: "region-a".into(),
        durability: Durability::Object,
        format_version: 1,
    }
}

pub fn byok(name: &str) -> PolicyTuple {
    PolicyTuple {
        key: KeyIdentity::Byok(name.into()),
        ..policy()
    }
}

pub fn memory() -> MemoryStore {
    MemoryStore::new(MemoryLimits {
        objects: 4096,
        versions: 16384,
        version_bytes: 64 << 20,
        identities: 16384,
        identity_bytes: 64 << 20,
    })
    .unwrap()
}

pub fn counted() -> Arc<CountingStore<MemoryStore>> {
    Arc::new(CountingStore::new(memory()))
}

pub fn put<'a>(index: Id, seq: u64, body: &'a [u8]) -> RecordSpec<'a> {
    RecordSpec {
        index,
        seq,
        op: RecordOp::Put,
        payload: PayloadRef::Inline(body),
    }
}

/// A store that can refuse mutations or fail identity resolution on demand.
pub struct Chaos<S> {
    pub inner: Arc<S>,
    /// Mutations still allowed; below zero every create/CAS is refused as unavailable.
    pub budget: AtomicI64,
    pub fail_resolve: AtomicBool,
}

impl<S> Chaos<S> {
    pub fn new(inner: Arc<S>) -> Self {
        Self {
            inner,
            budget: AtomicI64::new(i64::MAX),
            fail_resolve: AtomicBool::new(false),
        }
    }
    pub fn allow(&self, mutations: i64) {
        self.budget.store(mutations, Ordering::SeqCst);
    }
    fn spend(&self) -> bool {
        self.budget.fetch_sub(1, Ordering::SeqCst) > 0
    }
}

impl<S: ObjectBackend> ObjectBackend for Chaos<S> {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        if !self.spend() {
            return Box::pin(async { Err(StoreError::Unavailable) });
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

impl<S: MetadataStore> MetadataStore for Chaos<S> {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        if !self.spend() {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        self.inner.compare_exchange(c, r)
    }
    fn resolve<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        if self.fail_resolve.load(Ordering::SeqCst) {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        self.inner.resolve(c, r)
    }
}
