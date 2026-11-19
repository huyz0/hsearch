//! A store that holds payload objects of any size in memory, for histories at the document
//! ceiling: the in-memory store caps one object at 16 MiB. Every other key goes to the
//! fault-injecting store beneath it.
#![allow(dead_code)]
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use hsearch_object::{
    ByteRange, Error as StoreError, LargeObjectWriter, Metadata, MetadataStore, Mutation as Put,
    ObjectBackend, ObjectInfo, ObjectKey, Operation, Payload, RequestContext, VersionToken,
    WriteReceipt,
};

use crate::support::Crash;

fn is_payload(k: &ObjectKey) -> bool {
    k.as_bytes().windows(10).any(|w| w == b"/payloads/")
}

pub struct Big {
    pub inner: Arc<Crash>,
    pub objects: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    /// Payload requests that reach the store: creates, parts and completions.
    pub puts: AtomicU64,
    pub parts: AtomicU64,
    /// The largest part any request carried.
    pub largest_part: AtomicU64,
}

impl Big {
    pub fn new(inner: &Arc<Crash>) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::clone(inner),
            objects: Mutex::new(HashMap::new()),
            puts: AtomicU64::new(0),
            parts: AtomicU64::new(0),
            largest_part: AtomicU64::new(0),
        })
    }
    pub fn keys(&self) -> Vec<Vec<u8>> {
        let mut keys: Vec<_> = self.objects.lock().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }
    pub fn payload_requests(&self) -> u64 {
        self.puts.load(Ordering::SeqCst)
    }
}

pub struct BigUpload {
    key: Vec<u8>,
    data: Mutex<Vec<u8>>,
}

fn info(length: usize) -> ObjectInfo {
    ObjectInfo {
        version: VersionToken::new(b"v1").unwrap(),
        length: length as u64,
        metadata: Metadata::new(b"").unwrap(),
    }
}

impl ObjectBackend for Big {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        if !is_payload(k) {
            return self.inner.head(c, k);
        }
        Box::pin(async move {
            let objects = self.objects.lock().unwrap();
            objects
                .get(k.as_bytes())
                .map(|b| info(b.len()))
                .ok_or(StoreError::NotFound)
        })
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Put) -> Operation<'a, WriteReceipt> {
        if !is_payload(&r.key) {
            return self.inner.create(c, r);
        }
        Box::pin(async move {
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.parts.fetch_add(1, Ordering::SeqCst);
            self.largest_part
                .fetch_max(r.payload.len() as u64, Ordering::SeqCst);
            let mut objects = self.objects.lock().unwrap();
            if objects.contains_key(r.key.as_bytes()) {
                return Err(StoreError::Conflict);
            }
            objects.insert(r.key.as_bytes().to_vec(), r.payload.as_bytes().to_vec());
            Ok(WriteReceipt {
                request_id: r.request_id.clone(),
                object: info(r.payload.len()),
            })
        })
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        if !is_payload(k) {
            return self.inner.read_range(c, k, v, r);
        }
        Box::pin(async move {
            let objects = self.objects.lock().unwrap();
            let bytes = objects.get(k.as_bytes()).ok_or(StoreError::NotFound)?;
            let end = usize::try_from(r.offset() + r.length()).unwrap();
            if end > bytes.len() {
                return Err(StoreError::InvalidRange);
            }
            Payload::new(&bytes[usize::try_from(r.offset()).unwrap()..end])
        })
    }
}

impl MetadataStore for Big {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Put,
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
    fn resolve<'a>(&'a self, c: &'a RequestContext, r: &'a Put) -> Operation<'a, WriteReceipt> {
        self.inner.resolve(c, r)
    }
}

impl LargeObjectWriter for Big {
    type Upload = BigUpload;
    fn begin_upload<'a>(
        &'a self,
        _: &'a RequestContext,
        k: &'a ObjectKey,
    ) -> Operation<'a, Self::Upload> {
        Box::pin(async move {
            self.puts.fetch_add(1, Ordering::SeqCst);
            Ok(BigUpload {
                key: k.as_bytes().to_vec(),
                data: Mutex::new(Vec::new()),
            })
        })
    }
    fn put_part<'a>(
        &'a self,
        _: &'a RequestContext,
        u: &'a mut Self::Upload,
        b: &'a [u8],
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            if b.is_empty() {
                return Err(StoreError::InvalidInput);
            }
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.parts.fetch_add(1, Ordering::SeqCst);
            self.largest_part
                .fetch_max(b.len() as u64, Ordering::SeqCst);
            u.data.get_mut().unwrap().extend_from_slice(b);
            Ok(())
        })
    }
    fn complete_upload<'a>(
        &'a self,
        _: &'a RequestContext,
        u: &'a Self::Upload,
    ) -> Operation<'a, (VersionToken, u64)> {
        Box::pin(async move {
            self.puts.fetch_add(1, Ordering::SeqCst);
            let mut objects = self.objects.lock().unwrap();
            if objects.contains_key(&u.key) {
                return Err(StoreError::Conflict);
            }
            let data = std::mem::take(&mut *u.data.lock().unwrap());
            let length = data.len() as u64;
            objects.insert(u.key.clone(), data);
            Ok((VersionToken::new(b"v1").unwrap(), length))
        })
    }
    fn abort_upload<'a>(&'a self, _: &'a RequestContext, _: &'a Self::Upload) -> Operation<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
