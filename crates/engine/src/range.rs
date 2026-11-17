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
    BlobStore, CachedBlobDirectory, Input, PageCache, PageCacheConfig, StoreProfile,
    blob_directory::not_found,
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
pub(crate) fn block_on<T>(future: impl Future<Output = T>) -> T {
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

/// Keep the kind: the cache and Lucene tell "no such file" and a refused request
/// apart from a transient failure worth retrying.
fn object_error(error: hsearch_object::Error) -> lucene_store::Error {
    use hsearch_object::Error as E;
    let kind = match error {
        E::NotFound => io::ErrorKind::NotFound,
        E::PermissionDenied => io::ErrorKind::PermissionDenied,
        E::InvalidInput | E::InvalidRange | E::BudgetExceeded => io::ErrorKind::InvalidInput,
        E::Throttled | E::DeadlineExceeded | E::Unavailable => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    lucene_store::Error::Io(io::Error::new(
        kind,
        format!("object read failed: {error:?}"),
    ))
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

/// The most bytes of read-through mapped files one cache holds open without a
/// demand-paging kernel facility: files past it fail to open instead of filling the
/// disk. Provisional default; size it to the cache's disk tier.
pub const DEFAULT_SPILL_BUDGET: u64 = 8 << 30;

/// The most bytes of files held whole on the heap at once (pinned files, one-page
/// files and, without demand paging, files below the spill threshold): a file past
/// it fails to open. This is what bounds heap residency when `userfaultfd` is
/// unavailable, whatever the number of open readers. Provisional default.
pub const DEFAULT_HEAP_BUDGET: u64 = 1 << 30;

/// The default byte budget of the disk tier. Provisional; size it to the volume.
pub const DEFAULT_DISK_BUDGET: u64 = 16 << 30;

/// How hsearch reads a remote index through lucene-rust's page cache. Every field
/// has a bounded default (a disk tier and a spill each have a finite budget, and
/// spilling happens only once `disk_dir` is set); [`RangeCacheSettings::config`] turns it
/// into the cache's own configuration after checking what the store allows.
#[derive(Clone, Debug)]
pub struct RangeCacheSettings {
    /// Bytes per cached page (a multiple of 4 KiB to be demand-paged).
    pub page_size: usize,
    /// The local disk tier, or `None` to keep fetched pages in memory only. Without
    /// it nothing is spilled: files that cannot be demand-paged are read whole.
    pub disk_dir: Option<std::path::PathBuf>,
    /// The disk tier's byte budget.
    pub disk_budget: u64,
    /// Fetch pages on first touch through `userfaultfd` where the kernel has it.
    /// `false` forces the bounded read-through fallback (for hosts where the
    /// faulting mapping is unwanted, and for testing that path anywhere).
    pub demand_paging: bool,
    /// Without demand paging, a file at least this long is streamed to an unlinked
    /// file in `disk_dir` and mapped, so the heap holds one chunk rather than the
    /// whole file. `None` reads every such file into the heap.
    pub spill_min_bytes: Option<u64>,
    /// The most bytes of such mapped files open at once; one past it fails to open.
    pub spill_budget: u64,
    /// The most bytes of files held whole on the heap at once; one past it fails
    /// to open. Must be nonzero and finite.
    pub heap_budget: u64,
    /// Measured cost of the store, fixing the read-ahead window so it does not
    /// vary run to run. `None` keeps the live tuner, whose window followed request
    /// timings and varied 6-36 MiB per open under jittery latency.
    pub store_profile: Option<StoreProfile>,
    /// A fixed read-ahead window in bytes, overriding the profile and the tuner.
    pub download_window: Option<usize>,
    /// The largest ranged request in bytes (at most 16 MiB).
    pub max_request: usize,
    /// Requests in flight for one read.
    pub concurrency: usize,
    /// Read every vector file (`.vec`, `.vex`, `.ivv`, `.dhn` and the codes) whole
    /// when a snapshot opens, so the first queries ask the store for nothing.
    /// Off by default: a snapshot opens by reading about 0.3 MiB of a 52 MiB
    /// index and each query then reads the pages it touches, where on it reads
    /// the whole index once at open (measured in `m23-03-disk-vectors.md`).
    /// lucene-rust keeps this switch process-wide
    /// (`lucene_store::read_advice::set_warm_random`): [`Snapshot::open_blob_settings`]
    /// sets it, so every range snapshot in a process shares the last value opened,
    /// and a node should use one value.
    pub warm_on_open: bool,
}

impl Default for RangeCacheSettings {
    fn default() -> Self {
        let lucene = PageCacheConfig::default();
        Self {
            page_size: lucene.page_size,
            disk_dir: None,
            disk_budget: DEFAULT_DISK_BUDGET,
            demand_paging: true,
            spill_min_bytes: lucene.spill_min_bytes,
            spill_budget: DEFAULT_SPILL_BUDGET,
            heap_budget: DEFAULT_HEAP_BUDGET,
            store_profile: None,
            download_window: None,
            max_request: lucene.max_request,
            concurrency: lucene.concurrency,
            warm_on_open: false,
        }
    }
}

impl RangeCacheSettings {
    /// The page cache's configuration, refused (`Error::Limit`) if a request or page
    /// could exceed the store's 16 MiB cap, or the spill budget is zero while
    /// spilling is on.
    pub fn config(&self) -> Result<PageCacheConfig> {
        if self.max_request as u64 > MAX_REQUEST
            || self.page_size as u64 > MAX_REQUEST
            || self.download_window.is_some_and(|w| w as u64 > MAX_REQUEST)
            || (self.spill_min_bytes.is_some() && self.spill_budget == 0)
            || self.heap_budget == 0
            || self.heap_budget == u64::MAX
            || (self.disk_dir.is_some() && self.disk_budget == u64::MAX)
        {
            return Err(Error::Limit);
        }
        Ok(PageCacheConfig {
            page_size: self.page_size,
            disk_dir: self.disk_dir.clone(),
            disk_budget: self.disk_budget,
            download_window: self.download_window,
            max_request: self.max_request,
            concurrency: self.concurrency,
            store_profile: self.store_profile,
            demand_paging: self.demand_paging,
            spill_min_bytes: self.spill_min_bytes,
            spill_budget: self.spill_budget,
            heap_budget: self.heap_budget,
            ..PageCacheConfig::default()
        })
    }
}

/// What a node exports about its remote-read path: which path runs, and what the
/// cache holds and has requested. A plain value to be turned into instruments.
#[derive(Clone, Copy, Debug)]
pub struct RangeCacheMetrics {
    /// Whether this process can demand-fill through `userfaultfd` (probed at the
    /// first call; recent kernels commonly refuse it to unprivileged containers).
    pub demand_paging_available: bool,
    /// Whether the cache is configured to use it where available.
    pub demand_paging_configured: bool,
    /// The cache's counters: requests, bytes, heap and spill residency.
    pub stats: lucene_store::PageCacheStats,
}

impl RangeCacheMetrics {
    /// Read the metrics of `cache` now.
    pub fn of(cache: &PageCache) -> Self {
        Self {
            demand_paging_available: lucene_store::demand_paging_available(),
            demand_paging_configured: cache.config().demand_paging,
            stats: cache.stats(),
        }
    }

    /// Whether files are demand-filled on this node: available and configured.
    pub fn demand_paging_active(&self) -> bool {
        self.demand_paging_available && self.demand_paging_configured
    }
}

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

    /// The manifest's immutable version, in the cache key of every page: a page
    /// cached under one version is never served for another, even when a rewritten
    /// object keeps its name and length. Answered from the manifest, no request.
    fn version(&self, name: &str) -> lucene_store::Result<Option<String>> {
        let bytes = self.entry(name)?.version.as_bytes();
        let mut hex = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            hex.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
            hex.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
        }
        Ok(Some(hex))
    }

    fn get(&self, name: &str) -> lucene_store::Result<Input> {
        let length = self.entry(name)?.length;
        let length = usize::try_from(length)
            .map_err(|_| object_error(hsearch_object::Error::BudgetExceeded))?;
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

/// The page-cache namespace of one index's committed state: `domain` (the tenant
/// or index) and `version` (the manifest's root version), length-prefixed so no
/// two pairs encode alike (`("a|b","c")` and `("a","b|c")` differ). Pages cached
/// for one domain or one version are never served for another; both must be
/// nonempty.
pub fn cache_namespace(domain: &str, version: &str) -> Result<String> {
    if domain.is_empty() || version.is_empty() {
        return Err(Error::Limit);
    }
    Ok(format!(
        "{}:{domain}/{}:{version}",
        domain.len(),
        version.len()
    ))
}

impl Snapshot {
    /// Open the committed segments in `store` read-only, served by range through
    /// `cache`: a page is fetched when a query first touches it (or ahead of it by
    /// the cache's read-ahead) and the cache's budgets bound what stays resident.
    /// `namespace` tells this store's files apart in a cache shared by several.
    /// Opening costs whatever `store.list_with_sizes` costs, and nothing else if
    /// it answers from a manifest, as [`ObjectBlobStore`] does.
    ///
    /// The cache keys a page by namespace, file name, length and the store's
    /// [`BlobStore::version`]; [`ObjectBlobStore`] reports the manifest's, so two
    /// manifests never share pages. A store without `version` is keyed by name and
    /// length alone, and a disk tier can outlive a snapshot: derive `namespace` from
    /// the manifest (its root version) for such a store. A cache whose requests could exceed the store's 16 MiB cap is refused.
    pub fn open_blob<S: BlobStore + 'static>(
        store: S,
        cache: Arc<PageCache>,
        namespace: &str,
        schema: Schema,
    ) -> Result<Self> {
        let config = cache.config();
        if config.max_request as u64 > MAX_REQUEST || config.page_size as u64 > MAX_REQUEST {
            return Err(Error::Limit);
        }
        let directory = EngineDirectory::new(CachedBlobDirectory::new(store, cache, namespace));
        Snapshot::open_committed(&directory, schema)
    }

    /// [`Snapshot::open_blob_with`] from hsearch's own [`RangeCacheSettings`].
    pub fn open_blob_settings<S: BlobStore + 'static>(
        store: S,
        settings: &RangeCacheSettings,
        schema: Schema,
    ) -> Result<Self> {
        let config = settings.config()?;
        lucene_store::read_advice::set_warm_random(settings.warm_on_open);
        Self::open_blob_with(store, config, schema)
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

#[cfg(test)]
mod namespace_tests {
    use super::*;

    #[test]
    fn domain_and_version_are_both_in_the_key_without_ambiguity() {
        let a = cache_namespace("t1", "v1").unwrap();
        assert_ne!(a, cache_namespace("t2", "v1").unwrap());
        assert_ne!(a, cache_namespace("t1", "v2").unwrap());
        assert_ne!(
            cache_namespace("a/1:b", "c").unwrap(),
            cache_namespace("a", "1:b/1:c").unwrap()
        );
        assert_eq!(cache_namespace("", "v").unwrap_err(), Error::Limit);
        assert_eq!(cache_namespace("d", "").unwrap_err(), Error::Limit);
    }
}
