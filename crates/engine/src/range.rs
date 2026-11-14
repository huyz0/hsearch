//! Range-native remote reads (M02.03), by direct reuse of lucene-rust: its
//! `PageCache` (fixed pages, adjacent misses merged into one ranged request,
//! one request per page across readers, a bounded disk tier), its demand-filled
//! inputs and its range-reading `CachedBlobDirectory` already do the work, so
//! this crate adds no cache and no trace of its own.
//!
//! What a backend supplies is a `lucene_store::BlobStore`. [`ObjectBlobStore`] is
//! that trait over `hsearch_object::ObjectBackend`, as small as the two APIs
//! allow: names, lengths and version tokens come from the published manifest (so
//! reading costs no LIST and no HEAD, and every range read is pinned to the
//! version the manifest names), and each `get_range` is one `read_range` under a
//! fresh bounded request context. Attribution stays where the request is made, in
//! the backend's own ledger; the cache adds `PageCache::stats`.
//!
//! The store is read-only: writes belong to the publish path, not to a snapshot.
use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use hsearch_object::{
    Budget, ByteRange, Cancellation, ObjectBackend, ObjectKey, RequestContext, VersionToken,
};
use lucene_store::{
    BlobStore, CachedBlobDirectory, Input, PageCache, PageCacheConfig, blob_directory::not_found,
};

use crate::{EngineDirectory, Error, Result, Schema, Snapshot};

/// One published file: the object that holds it, its exact version and length.
#[derive(Clone, Debug)]
pub struct ManifestEntry {
    /// The object key.
    pub key: ObjectKey,
    /// The immutable version every read is pinned to.
    pub version: VersionToken,
    /// The file's exact length in bytes.
    pub length: u64,
}

/// A read-only `BlobStore` over an `ObjectBackend` and a published manifest.
pub struct ObjectBlobStore<B: ObjectBackend> {
    backend: B,
    files: BTreeMap<String, ManifestEntry>,
    deadline: Duration,
}

struct Unpark(std::thread::Thread);
impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Run one backend future to completion on this thread. The futures here are
/// one provider call each; there is no runtime to enter.
fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::park();
    }
}

fn object_error(error: hsearch_object::Error) -> lucene_store::Error {
    lucene_store::Error::Io(io::Error::other(format!("object read failed: {error:?}")))
}

fn read_only() -> lucene_store::Error {
    lucene_store::Error::Io(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "snapshot store is read-only",
    ))
}

/// The most one request may return; a larger page or request is refused rather
/// than allocated.
const MAX_REQUEST: u64 = 16 * 1024 * 1024;

impl<B: ObjectBackend> ObjectBlobStore<B> {
    /// `files` maps each Lucene file name to its published object. `deadline`
    /// bounds every single read (at most five minutes).
    pub fn new(
        backend: B,
        files: impl IntoIterator<Item = (String, ManifestEntry)>,
        deadline: Duration,
    ) -> io::Result<Self> {
        let files: BTreeMap<String, ManifestEntry> = files.into_iter().collect();
        let valid =
            |n: &str| !n.is_empty() && n != "." && n != ".." && !n.contains(['/', '\\', '\0']);
        if !files.keys().all(|n| valid(n)) || deadline > Duration::from_secs(300) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "manifest names must be single path components and the deadline at most 5 min",
            ));
        }
        Ok(Self {
            backend,
            files,
            deadline,
        })
    }

    /// The backend, for its own counters and ledger.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    fn entry(&self, name: &str) -> lucene_store::Result<&ManifestEntry> {
        self.files.get(name).ok_or_else(|| not_found(name))
    }
}

impl<B: ObjectBackend> BlobStore for ObjectBlobStore<B> {
    fn list(&self) -> lucene_store::Result<Vec<String>> {
        Ok(self.files.keys().cloned().collect())
    }

    fn list_with_sizes(&self) -> lucene_store::Result<Option<Vec<(String, u64)>>> {
        Ok(Some(
            self.files
                .iter()
                .map(|(n, e)| (n.clone(), e.length))
                .collect(),
        ))
    }

    fn size(&self, name: &str) -> lucene_store::Result<u64> {
        Ok(self.entry(name)?.length)
    }

    fn get(&self, name: &str) -> lucene_store::Result<Input> {
        let length = self.entry(name)?.length;
        let length = usize::try_from(length).map_err(|_| read_only())?;
        Ok(Input::Owned(self.get_range(name, 0, length)?))
    }

    fn get_range(&self, name: &str, offset: u64, len: usize) -> lucene_store::Result<Vec<u8>> {
        let entry = self.entry(name)?;
        // Fewer bytes where the object ends first, as the trait says.
        let start = offset.min(entry.length);
        let take = (entry.length - start).min(len as u64);
        if take == 0 {
            return Ok(Vec::new());
        }
        if take > MAX_REQUEST {
            return Err(object_error(hsearch_object::Error::BudgetExceeded));
        }
        let range = ByteRange::new(start, take).map_err(object_error)?;
        // One context per request: its budget is exactly this read, so a read can
        // neither run past its deadline nor return more than it asked for.
        let context = RequestContext::new(
            self.deadline,
            Cancellation::default(),
            Budget {
                request_bytes: 4096 + entry.key.len() as u64 + entry.version.len() as u64,
                response_bytes: take,
                attempts: 8,
            },
        )
        .map_err(object_error)?;
        let payload =
            block_on(
                self.backend
                    .read_range(&context, &entry.key, &entry.version, range),
            )
            .map_err(object_error)?;
        if payload.len() as u64 != take {
            return Err(object_error(hsearch_object::Error::Unavailable));
        }
        Ok(payload.as_bytes().to_vec())
    }

    fn put(&self, _name: &str, _bytes: Vec<u8>) -> lucene_store::Result<()> {
        Err(read_only())
    }

    fn delete(&self, _name: &str) -> lucene_store::Result<()> {
        Err(read_only())
    }
}

impl Snapshot {
    /// Open the committed segments in `store` read-only, served by range through
    /// `cache`: a page is fetched when a query first touches it (or ahead of it by
    /// the cache's read-ahead) and the cache's budgets bound what stays resident.
    /// `namespace` tells this store's files apart in a cache shared by several.
    /// Opening costs whatever `store.list_with_sizes` costs, and nothing else if
    /// it answers from a manifest, as [`ObjectBlobStore`] does.
    pub fn open_blob<S: BlobStore + 'static>(
        store: S,
        cache: Arc<PageCache>,
        namespace: &str,
        schema: Schema,
    ) -> Result<Self> {
        let directory = EngineDirectory::new(CachedBlobDirectory::new(store, cache, namespace));
        Snapshot::open_committed(&directory, schema)
    }

    /// [`Snapshot::open_blob`] with a private cache built from `config`.
    pub fn open_blob_with<S: BlobStore + 'static>(
        store: S,
        config: PageCacheConfig,
        schema: Schema,
    ) -> Result<Self> {
        let cache = PageCache::new(config).map_err(|_| Error::Native)?;
        Self::open_blob(store, Arc::new(cache), "snapshot", schema)
    }
}
