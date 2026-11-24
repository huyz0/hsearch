//! A store that records every request it serves, by kind and key.
#![allow(dead_code)]
use std::sync::{Arc, Mutex};

use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    ByteRange, LargeObjectWriter, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey,
    Operation, Payload, RequestContext, VersionToken, WriteReceipt,
};

pub struct Counting {
    shards: Vec<MemoryStore>,
    log: Mutex<Vec<(&'static str, String)>>,
}

impl Counting {
    pub fn new() -> Arc<Self> {
        Self::sharded(1)
    }
    /// `n` memory stores, each key living wholly on the one its bytes hash to, for a test that
    /// writes more objects than one `MemoryStore`'s fixed cap holds. Uploads need `n == 1`.
    pub fn sharded(n: usize) -> Arc<Self> {
        let shards = (0..n)
            .map(|_| {
                MemoryStore::new(MemoryLimits {
                    objects: 4096,
                    versions: 16384,
                    version_bytes: 64 << 20,
                    identities: 16384,
                    identity_bytes: 64 << 20,
                })
                .unwrap()
            })
            .collect();
        Arc::new(Self {
            shards,
            log: Mutex::new(Vec::new()),
        })
    }
    fn at(&self, key: &ObjectKey) -> &MemoryStore {
        let sum = key.as_bytes().iter().fold(0usize, |h, b| {
            h.wrapping_mul(31).wrapping_add(usize::from(*b))
        });
        &self.shards[sum % self.shards.len()]
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
        self.at(key).head(c, key)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        self.note("create", &r.key);
        self.at(&r.key).create(c, r)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        key: &'a ObjectKey,
        version: &'a VersionToken,
        range: ByteRange,
    ) -> Operation<'a, Payload> {
        self.note("get", key);
        self.at(key).read_range(c, key, version, range)
    }
}

impl MetadataStore for Counting {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.note("cas", &r.key);
        self.at(&r.key).compare_exchange(c, r)
    }
    fn resolve<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.note("resolve", &r.key);
        self.at(&r.key).resolve(c, r)
    }
}

impl LargeObjectWriter for Counting {
    type Upload = <MemoryStore as LargeObjectWriter>::Upload;
    fn begin_upload<'a>(
        &'a self,
        c: &'a RequestContext,
        key: &'a ObjectKey,
    ) -> Operation<'a, Self::Upload> {
        self.note("begin", key);
        self.at(key).begin_upload(c, key)
    }
    fn put_part<'a>(
        &'a self,
        c: &'a RequestContext,
        upload: &'a mut Self::Upload,
        bytes: &'a [u8],
    ) -> Operation<'a, ()> {
        self.shards[0].put_part(c, upload, bytes)
    }
    fn complete_upload<'a>(
        &'a self,
        c: &'a RequestContext,
        upload: &'a Self::Upload,
    ) -> Operation<'a, (VersionToken, u64)> {
        self.shards[0].complete_upload(c, upload)
    }
    fn abort_upload<'a>(
        &'a self,
        c: &'a RequestContext,
        upload: &'a Self::Upload,
    ) -> Operation<'a, ()> {
        self.shards[0].abort_upload(c, upload)
    }
}
