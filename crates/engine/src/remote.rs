//! A directory whose immutable files live in a remote store and are fetched
//! whole into a bounded, disposable local cache.
//!
//! The engine never names a store: [`RemoteFiles`] is the seam, and a storage
//! crate implements it over its object backend. The cache holds nothing that is
//! not also remote, so deleting it, evicting from it or losing it never changes
//! what is searchable. Nothing here lists the remote: the set of files is the
//! manifest the caller passes in, so opening costs one fetch per file and no
//! LIST. Range-native reads arrive later; this fetches whole files.
//!
//! v0 limits: the cache directory is cleared when the directory is created (a
//! cache surviving a restart is later work), and a fetch holds the cache lock,
//! so concurrent opens of different files queue.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use lucene_store::{DataOutput, Directory, FsDirectory, FsIndexOutput, Input, Lock};

/// Why a whole-file fetch failed. No provider text, request or payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteError {
    /// The named file is not in the store.
    NotFound,
    /// The store could not serve the file now; retrying later may succeed.
    Unavailable,
}

/// Whole-file source for immutable segment files.
pub trait RemoteFiles: Send + Sync {
    /// Return exactly the stored bytes of `name`, or fail. A partial file is
    /// never returned.
    fn fetch(&self, name: &str) -> Result<Vec<u8>, RemoteError>;
}

#[derive(Default)]
struct Cache {
    /// Remote-backed names currently on local disk, least recently used first.
    order: VecDeque<String>,
    sizes: BTreeMap<String, u64>,
    bytes: u64,
}

/// Directory over a remote manifest with a bounded local cache.
pub struct CachedDirectory<R: RemoteFiles> {
    local: FsDirectory,
    remote: R,
    root: PathBuf,
    max_bytes: u64,
    /// Names that exist remotely. Only these are ever evicted from the cache.
    backed: Mutex<BTreeSet<String>>,
    cache: Mutex<Cache>,
    fetches: AtomicU64,
}

fn io_error(kind: io::ErrorKind, what: &str) -> lucene_store::Error {
    lucene_store::Error::Io(io::Error::new(kind, what.to_owned()))
}

fn poisoned() -> lucene_store::Error {
    io_error(io::ErrorKind::Other, "cache lock poisoned")
}

impl<R: RemoteFiles> CachedDirectory<R> {
    /// Create the directory over `cache_root`, which is created if absent and
    /// emptied if not: stale cache bytes are never trusted. `files` is the
    /// manifest of remote file names; `max_bytes` bounds the cached bytes.
    pub fn new(
        cache_root: impl Into<PathBuf>,
        remote: R,
        files: impl IntoIterator<Item = String>,
        max_bytes: u64,
    ) -> io::Result<Self> {
        let root = cache_root.into();
        if root.exists() {
            for entry in std::fs::read_dir(&root)? {
                let path = entry?.path();
                if path.is_dir() {
                    std::fs::remove_dir_all(&path)?;
                } else {
                    std::fs::remove_file(&path)?;
                }
            }
        } else {
            std::fs::create_dir_all(&root)?;
        }
        Ok(Self {
            local: FsDirectory::open(root.clone()),
            remote,
            root,
            max_bytes,
            backed: Mutex::new(files.into_iter().collect()),
            cache: Mutex::new(Cache::default()),
            fetches: AtomicU64::new(0),
        })
    }

    /// Whole-file fetches issued so far, one per cache miss.
    #[must_use]
    pub fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::Relaxed)
    }

    /// Bytes currently held in the cache for remote-backed files.
    #[must_use]
    pub fn cached_bytes(&self) -> u64 {
        self.cache.lock().map_or(0, |cache| cache.bytes)
    }

    /// The cache directory, for tests and operators.
    #[must_use]
    pub fn cache_root(&self) -> &Path {
        &self.root
    }

    /// Declare that `name` is now in the remote store, making it evictable and
    /// refetchable. Call only after the store acknowledged the file.
    pub fn mark_remote(&self, name: &str) -> io::Result<()> {
        self.backed
            .lock()
            .map_err(|_| io::Error::other("manifest lock poisoned"))?
            .insert(name.to_owned());
        Ok(())
    }

    /// Drop every cached remote-backed file. Always safe.
    pub fn evict_all(&self) -> lucene_store::Result<()> {
        let mut cache = self.cache.lock().map_err(|_| poisoned())?;
        while let Some(name) = cache.order.pop_front() {
            remove_cached(&self.local, &mut cache, &name)?;
        }
        Ok(())
    }

    fn fetch_into_cache(&self, name: &str, cache: &mut Cache) -> lucene_store::Result<Input> {
        self.fetches.fetch_add(1, Ordering::Relaxed);
        let bytes = self.remote.fetch(name).map_err(|error| match error {
            RemoteError::NotFound => io_error(io::ErrorKind::NotFound, "remote file not found"),
            RemoteError::Unavailable => io_error(io::ErrorKind::Other, "remote store unavailable"),
        })?;
        let length = bytes.len() as u64;
        if length > self.max_bytes {
            return Err(io_error(
                io::ErrorKind::OutOfMemory,
                "file larger than the cache bound",
            ));
        }
        while cache.bytes + length > self.max_bytes {
            let Some(victim) = cache.order.pop_front() else {
                break;
            };
            remove_cached(&self.local, cache, &victim)?;
        }
        let mut out = self.local.create_output(name)?;
        out.write_bytes(&bytes);
        out.close()?;
        cache.order.push_back(name.to_owned());
        cache.sizes.insert(name.to_owned(), length);
        cache.bytes += length;
        Ok(Input::Owned(bytes))
    }
}

fn remove_cached(local: &FsDirectory, cache: &mut Cache, name: &str) -> lucene_store::Result<()> {
    cache.bytes -= cache.sizes.remove(name).unwrap_or(0);
    cache.order.retain(|n| n != name);
    match local.delete_file(name) {
        Ok(()) => Ok(()),
        Err(lucene_store::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

impl<R: RemoteFiles> Directory for CachedDirectory<R> {
    fn list_all(&self) -> lucene_store::Result<Vec<String>> {
        let mut names: BTreeSet<String> = self.local.list_all()?.into_iter().collect();
        names.extend(self.backed.lock().map_err(|_| poisoned())?.iter().cloned());
        Ok(names.into_iter().collect())
    }

    fn open(&self, name: &str) -> lucene_store::Result<Input> {
        let remote_backed = self.backed.lock().map_err(|_| poisoned())?.contains(name);
        if !remote_backed {
            return self.local.open(name);
        }
        let mut cache = self.cache.lock().map_err(|_| poisoned())?;
        if cache.sizes.contains_key(name) {
            cache.order.retain(|n| n != name);
            cache.order.push_back(name.to_owned());
            return self.local.open(name);
        }
        self.fetch_into_cache(name, &mut cache)
    }

    fn create_output(&self, name: &str) -> lucene_store::Result<FsIndexOutput> {
        self.local.create_output(name)
    }

    fn create_temp_output(
        &self,
        prefix: &str,
        suffix: &str,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.local.create_temp_output(prefix, suffix)
    }

    fn sync(&self, names: &[String]) -> lucene_store::Result<()> {
        self.local.sync(names)
    }

    fn rename(&self, source: &str, dest: &str) -> lucene_store::Result<()> {
        self.local.rename(source, dest)
    }

    /// Removing a remote-backed file forgets it locally; the store's copy is
    /// collected by a separate, deliberate step, never by an engine merge.
    fn delete_file(&self, name: &str) -> lucene_store::Result<()> {
        let removed = self.backed.lock().map_err(|_| poisoned())?.remove(name);
        if removed {
            let mut cache = self.cache.lock().map_err(|_| poisoned())?;
            remove_cached(&self.local, &mut cache, name)
        } else {
            self.local.delete_file(name)
        }
    }

    fn sync_meta_data(&self) -> lucene_store::Result<()> {
        self.local.sync_meta_data()
    }

    fn obtain_lock(&self, name: &str) -> lucene_store::Result<Box<dyn Lock>> {
        self.local.obtain_lock(name)
    }

    fn pending_deletions(&self) -> lucene_store::Result<BTreeSet<String>> {
        self.local.pending_deletions()
    }

    fn fs_directory_path(&self) -> Option<&Path> {
        self.local.fs_directory_path()
    }
}
