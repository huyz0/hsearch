//! A store that records every request it serves, by kind and key.
#![allow(dead_code)]
use std::sync::{Arc, Mutex};

use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    ByteRange, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey, Operation, Payload,
    RequestContext, VersionToken, WriteReceipt,
};

pub struct Counting {
    inner: MemoryStore,
    log: Mutex<Vec<(&'static str, String)>>,
}

impl Counting {
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
            log: Mutex::new(Vec::new()),
        })
    }
    fn note(&self, kind: &'static str, key: &ObjectKey) {
        let key = String::from_utf8_lossy(key.as_bytes()).into_owned();
        self.log.lock().unwrap().push((kind, key));
    }
    pub fn take(&self) -> Vec<(&'static str, String)> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }
}

impl ObjectBackend for Counting {
    fn head<'a>(&'a self, c: &'a RequestContext, key: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.note("head", key);
        self.inner.head(c, key)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        self.note("create", &r.key);
        self.inner.create(c, r)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        key: &'a ObjectKey,
        version: &'a VersionToken,
        range: ByteRange,
    ) -> Operation<'a, Payload> {
        self.note("get", key);
        self.inner.read_range(c, key, version, range)
    }
}

impl MetadataStore for Counting {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.note("cas", &r.key);
        self.inner.compare_exchange(c, r)
    }
    fn resolve<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.note("resolve", &r.key);
        self.inner.resolve(c, r)
    }
}
