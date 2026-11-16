//! Request accounting over any [`MetadataStore`]: every logical call the lane code makes, by verb,
//! with the bytes each carried. This counts **calls into the store**; the S3 adapter's own counters
//! (`S3Store::counts`) count wire attempts including retries, and the benchmarks report both.
use std::sync::atomic::{AtomicU64, Ordering};

use hsearch_object::{
    ByteRange, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey, Operation, Payload,
    RequestContext, VersionToken, WriteReceipt,
};

/// A snapshot of calls made through a [`CountingStore`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Requests {
    /// `head` calls.
    pub head: u64,
    /// Ranged reads.
    pub read: u64,
    /// Conditional creates.
    pub create: u64,
    /// Compare-exchange calls.
    pub cas: u64,
    /// Identity resolutions (after a conflict or lost reply).
    pub resolve: u64,
    /// Version removals.
    pub remove: u64,
    /// Request body bytes sent by creates and CAS.
    pub put_bytes: u64,
    /// Bytes returned by ranged reads.
    pub get_bytes: u64,
}

impl Requests {
    /// Object PUTs: creates plus compare-exchanges.
    #[must_use]
    pub fn puts(&self) -> u64 {
        self.create + self.cas
    }

    /// Object GETs and HEADs.
    #[must_use]
    pub fn reads(&self) -> u64 {
        self.head + self.read
    }

    /// Every call.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.head + self.read + self.create + self.cas + self.resolve + self.remove
    }

    /// The calls made since `before`.
    #[must_use]
    pub fn since(&self, before: &Requests) -> Requests {
        Requests {
            head: self.head - before.head,
            read: self.read - before.read,
            create: self.create - before.create,
            cas: self.cas - before.cas,
            resolve: self.resolve - before.resolve,
            remove: self.remove - before.remove,
            put_bytes: self.put_bytes - before.put_bytes,
            get_bytes: self.get_bytes - before.get_bytes,
        }
    }
}

#[derive(Default)]
struct Counters {
    head: AtomicU64,
    read: AtomicU64,
    create: AtomicU64,
    cas: AtomicU64,
    resolve: AtomicU64,
    remove: AtomicU64,
    put_bytes: AtomicU64,
    get_bytes: AtomicU64,
}

/// Wraps a store and counts the calls made through it.
pub struct CountingStore<S> {
    inner: S,
    counters: Counters,
}

impl<S> CountingStore<S> {
    /// Count calls to `inner`.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            counters: Counters::default(),
        }
    }

    /// The wrapped store.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Calls so far.
    pub fn requests(&self) -> Requests {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Requests {
            head: load(&self.counters.head),
            read: load(&self.counters.read),
            create: load(&self.counters.create),
            cas: load(&self.counters.cas),
            resolve: load(&self.counters.resolve),
            remove: load(&self.counters.remove),
            put_bytes: load(&self.counters.put_bytes),
            get_bytes: load(&self.counters.get_bytes),
        }
    }
}

impl<S: ObjectBackend> ObjectBackend for CountingStore<S> {
    fn head<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
    ) -> Operation<'a, ObjectInfo> {
        self.counters.head.fetch_add(1, Ordering::Relaxed);
        self.inner.head(context, key)
    }

    fn create<'a>(
        &'a self,
        context: &'a RequestContext,
        request: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.counters.create.fetch_add(1, Ordering::Relaxed);
        self.counters
            .put_bytes
            .fetch_add(request.payload.len() as u64, Ordering::Relaxed);
        self.inner.create(context, request)
    }

    fn read_range<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
        version: &'a VersionToken,
        range: ByteRange,
    ) -> Operation<'a, Payload> {
        self.counters.read.fetch_add(1, Ordering::Relaxed);
        let counters = &self.counters;
        let inner = self.inner.read_range(context, key, version, range);
        Box::pin(async move {
            let got = inner.await?;
            counters
                .get_bytes
                .fetch_add(got.len() as u64, Ordering::Relaxed);
            Ok(got)
        })
    }
}

impl<S: MetadataStore> MetadataStore for CountingStore<S> {
    fn compare_exchange<'a>(
        &'a self,
        context: &'a RequestContext,
        request: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.counters.cas.fetch_add(1, Ordering::Relaxed);
        self.counters
            .put_bytes
            .fetch_add(request.payload.len() as u64, Ordering::Relaxed);
        self.inner.compare_exchange(context, request)
    }

    fn resolve<'a>(
        &'a self,
        context: &'a RequestContext,
        request: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.counters.resolve.fetch_add(1, Ordering::Relaxed);
        self.inner.resolve(context, request)
    }

    fn remove_version<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
        version: &'a VersionToken,
    ) -> Operation<'a, ()> {
        self.counters.remove.fetch_add(1, Ordering::Relaxed);
        self.inner.remove_version(context, key, version)
    }
}
