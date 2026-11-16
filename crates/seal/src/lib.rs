//! An encrypting decorator over [`ObjectBackend`] (M05.04): every object it creates is sealed in the block
//! format of `hsearch_security::block` under a fresh data key wrapped by the index's customer key, and
//! every byte it returns has authenticated.
//!
//! * **Create.** One provider wrap per object, never per block. The object id is derived from the object key,
//!   so a sealed object copied to any other key, tenant or domain fails to open. Sealing is deterministic from
//!   (data key, object, plaintext), and the decorator remembers the sealed request by its identity, so a
//!   retry resends **identical bytes**: the store resolves it to the original receipt, and a retry that
//!   changes the plaintext is an [`Error::IdentityConflict`]. If the memory of a request was evicted, the
//!   retry seals under a new key and the store refuses it as an identity conflict: it fails closed, it never
//!   encrypts two plaintexts under one key and nonce.
//! * **Head and read.** `head` reports the *plaintext* length. It reads the header once and caches the opened
//!   object, so a read after a head is one backend request for exactly the blocks the range touches. A read
//!   of an uncached object costs a head and a header read first.
//! * **Warm objects are rechecked.** Every head and read resolves the scope again and asks the key manager
//!   again under the current policy generation, so revocation, invalidation and the key-cache lifetime reach
//!   an object the decorator already opened.
//! * **Blocking.** Key-provider calls and sealing are synchronous (the `KeyProvider` contract is); an async
//!   runtime runs a network provider adapter off its executor threads.
//! * **Not covered.** [`hsearch_object::MetadataStore`] mutations (`compare_exchange`, `resolve`) are not
//!   decorated: conditional replacement has no fresh-key-per-object form, and the M03 lane and head bodies are
//!   covered by the format envelope's capability bit, not by this crate. Object metadata is opaque caller data
//!   stored in the clear; sensitive values belong in the sealed body.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use hsearch_object::{
    ByteRange, Error, Mutation, ObjectBackend, ObjectInfo, ObjectKey, Operation, Payload,
    Precondition, RequestContext, RequestId, VersionToken, WriteReceipt,
};
use hsearch_security::block::{
    HEAD_LEN, Header, MAX_BLOCK_SIZE, MAX_HEADER_LEN, Opener, TAG_LEN, object_id, seal_object,
};
use hsearch_security::ids::IndexId;
use hsearch_security::keys::{CallOptions, KeyContext, KeyRef, ProviderError, WrappedKey};
use hsearch_security::manager::{KeyError, KeyManager};
use sha2::{Digest, Sha256};

/// The largest response a single backend read may carry (the object contract's payload bound).
const MAX_PAYLOAD: u64 = 16 * 1024 * 1024;

/// What an object key belongs to: the index (tenant), its crypto domain, its own customer key if it has
/// one, and the policy generation the key cache is keyed by.
#[derive(Clone, Debug)]
pub struct Scope {
    /// The index (tenant).
    pub index: IndexId,
    /// The crypto domain.
    pub domain: [u8; 16],
    /// The index's own key reference; `None` selects the deployment default.
    pub key: Option<KeyRef>,
    /// The index policy generation.
    pub generation: u64,
}

/// Maps an object key to its scope. An unknown key is refused, never given a default scope.
pub trait ScopeResolver: Send + Sync {
    /// The scope, or the refusal to return (usually [`Error::PermissionDenied`]).
    fn scope(&self, key: &ObjectKey) -> Result<Scope, Error>;
}

/// Sizes of the decorator and its bounded memories.
#[derive(Clone, Copy, Debug)]
pub struct SealConfig {
    /// Plaintext bytes per block, `1..=1 MiB`. A point read decrypts a whole block.
    pub block_size: u32,
    /// Sealed requests remembered for retry.
    pub retry_entries: usize,
    /// Sealed bytes remembered for retry.
    pub retry_bytes: usize,
    /// Opened objects remembered.
    pub open_entries: usize,
}

struct Retry {
    fingerprint: [u8; 32],
    sealed: Mutation,
    plain_len: u64,
}

struct OpenedObject {
    opener: Opener,
    plain_len: u64,
    wrapped: WrappedKey,
}

/// A small FIFO-evicting map with an entry and a byte bound.
struct Bounded<K, V> {
    map: HashMap<K, (V, usize)>,
    order: VecDeque<K>,
    entries: usize,
    bytes: usize,
    used: usize,
}

impl<K: Clone + Eq + std::hash::Hash, V: Clone> Bounded<K, V> {
    fn new(entries: usize, bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            entries,
            bytes,
            used: 0,
        }
    }

    fn get(&self, key: &K) -> Option<V> {
        self.map.get(key).map(|(v, _)| v.clone())
    }

    /// Inserts, evicting oldest first; a value larger than the whole byte bound is not kept.
    fn insert(&mut self, key: K, value: V, size: usize) {
        if size > self.bytes {
            return;
        }
        if let Some((_, old)) = self.map.remove(&key) {
            self.used -= old;
            self.order.retain(|k| k != &key);
        }
        while self.map.len() >= self.entries || self.used + size > self.bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some((_, old)) = self.map.remove(&oldest) {
                self.used -= old;
            }
        }
        self.used += size;
        self.order.push_back(key.clone());
        self.map.insert(key, (value, size));
    }
}

/// The encrypting decorator. See the module documentation.
pub struct SealedBackend<B> {
    inner: B,
    keys: Arc<KeyManager>,
    scopes: Arc<dyn ScopeResolver>,
    block_size: u32,
    retries: Mutex<Bounded<RequestId, Arc<Retry>>>,
    opened: Mutex<Bounded<(ObjectKey, VersionToken), Arc<OpenedObject>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned memory is still a valid memory: its worst case is a missed hit.
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn key_error(e: KeyError) -> Error {
    match e {
        KeyError::NotAllowed | KeyError::KeyUnusable => Error::PermissionDenied,
        KeyError::Provider(p) => match p {
            ProviderError::Denied | ProviderError::Revoked | ProviderError::Destroyed => {
                Error::PermissionDenied
            }
            ProviderError::Throttled { .. } => Error::Throttled,
            ProviderError::Timeout | ProviderError::Unavailable => Error::Unavailable,
            ProviderError::Cancelled => Error::Cancelled,
            ProviderError::Unsupported => Error::Unsupported,
            ProviderError::NotFound
            | ProviderError::VersionMismatch
            | ProviderError::ContextMismatch
            | ProviderError::Corrupt => Error::Corrupt,
            // A provider failure this build does not know is unavailable, never success.
            _ => Error::Unavailable,
        },
        _ => Error::Unavailable,
    }
}

impl<B: ObjectBackend> SealedBackend<B> {
    /// Wraps `inner`. Refuses a block size outside `1..=1 MiB` and a zero-sized memory.
    pub fn new(
        inner: B,
        keys: Arc<KeyManager>,
        scopes: Arc<dyn ScopeResolver>,
        config: SealConfig,
    ) -> Result<Self, Error> {
        if config.block_size == 0
            || config.block_size > MAX_BLOCK_SIZE
            || config.retry_entries == 0
            || config.retry_bytes == 0
            || config.open_entries == 0
        {
            return Err(Error::InvalidInput);
        }
        Ok(Self {
            inner,
            keys,
            scopes,
            block_size: config.block_size,
            retries: Mutex::new(Bounded::new(config.retry_entries, config.retry_bytes)),
            // Opened objects are small (a header and a key schedule); bound them by count alone.
            opened: Mutex::new(Bounded::new(config.open_entries, usize::MAX)),
        })
    }

    fn fingerprint(request: &Mutation) -> [u8; 32] {
        let mut h = Sha256::new();
        for part in [
            request.key.as_bytes(),
            request.payload.as_bytes(),
            request.metadata.as_bytes(),
        ] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part);
        }
        h.finalize().into()
    }

    /// Seals a request, or returns the sealed form remembered for its identity.
    fn seal(&self, request: &Mutation) -> Result<Arc<Retry>, Error> {
        if request.precondition != Precondition::Absent {
            return Err(Error::InvalidInput);
        }
        let scope = self.scopes.scope(&request.key)?;
        let fingerprint = Self::fingerprint(request);
        if let Some(found) = lock(&self.retries).get(&request.request_id) {
            return if found.fingerprint == fingerprint {
                Ok(found)
            } else {
                Err(Error::IdentityConflict)
            };
        }
        // The largest sealed form of this plaintext, checked before a key is made for it.
        let blocks = (request.payload.len() as u64).div_ceil(u64::from(self.block_size));
        if request.payload.len() as u64 + blocks * TAG_LEN as u64 + MAX_HEADER_LEN as u64
            > MAX_PAYLOAD
        {
            return Err(Error::InvalidInput);
        }
        let object = object_id(request.key.as_bytes());
        let context = KeyContext {
            index: scope.index.clone(),
            domain: scope.domain,
            object,
        };
        let (dek, wrapped) = self
            .keys
            .create_object_key(
                scope.key.as_ref(),
                &context,
                scope.generation,
                &CallOptions::default(),
            )
            .map_err(key_error)?;
        let body = seal_object(
            &dek,
            &wrapped,
            object,
            scope.domain,
            self.block_size,
            request.payload.as_bytes(),
        )
        .map_err(|_| Error::InvalidInput)?;
        let sealed = Mutation {
            payload: Payload::new(&body)?,
            ..request.clone()
        };
        let made = Arc::new(Retry {
            fingerprint,
            sealed,
            plain_len: request.payload.len() as u64,
        });
        // Two callers racing on one identity: the first sealed form wins and both send it.
        let mut retries = lock(&self.retries);
        if let Some(found) = retries.get(&request.request_id) {
            return if found.fingerprint == fingerprint {
                Ok(found)
            } else {
                Err(Error::IdentityConflict)
            };
        }
        let held =
            body.len() + request.key.len() + request.metadata.len() + request.request_id.len();
        retries.insert(request.request_id.clone(), made.clone(), held);
        Ok(made)
    }

    /// A warm object is not trusted past the key and scope that opened it: every use asks the key manager
    /// again (a cache hit there costs a lookup, and it honours invalidation, revocation and the cache
    /// lifetime) under the **current** scope and policy generation.
    fn recheck(&self, key: &ObjectKey, scope: &Scope, opened: &OpenedObject) -> Result<(), Error> {
        let kc = KeyContext {
            index: scope.index.clone(),
            domain: scope.domain,
            object: object_id(key.as_bytes()),
        };
        self.keys
            .open_object_key(
                &opened.wrapped,
                &kc,
                scope.generation,
                &CallOptions::default(),
            )
            .map(|_| ())
            .map_err(key_error)
    }

    /// The opened object for a key at a version whose full stored length is `stored_len`.
    async fn open(
        &self,
        context: &RequestContext,
        key: &ObjectKey,
        version: &VersionToken,
        stored_len: u64,
    ) -> Result<Arc<OpenedObject>, Error> {
        let cache_key = (key.clone(), version.clone());
        let scope = self.scopes.scope(key)?;
        if let Some(found) = lock(&self.opened).get(&cache_key) {
            if found.opener.object_len() != stored_len {
                return Err(Error::Corrupt);
            }
            self.recheck(key, &scope, &found)?;
            return Ok(found);
        }
        if stored_len < HEAD_LEN as u64 {
            return Err(Error::Corrupt);
        }
        let want = stored_len.min(MAX_HEADER_LEN as u64);
        let bytes = self
            .inner
            .read_range(context, key, version, ByteRange::new(0, want)?)
            .await?;
        let bytes = bytes.as_bytes();
        let total =
            Header::total_len(&bytes[..HEAD_LEN.min(bytes.len())]).map_err(|_| Error::Corrupt)?;
        if total > bytes.len() {
            return Err(Error::Corrupt);
        }
        let header = Header::parse(&bytes[..total]).map_err(|_| Error::Corrupt)?;
        let object = object_id(key.as_bytes());
        let kc = KeyContext {
            index: scope.index.clone(),
            domain: scope.domain,
            object,
        };
        let dek = self
            .keys
            .open_object_key(
                header.wrapped(),
                &kc,
                scope.generation,
                &CallOptions::default(),
            )
            .map_err(key_error)?;
        let opener = header
            .open(&dek, object, scope.domain)
            .map_err(|_| Error::Corrupt)?;
        if opener.object_len() != stored_len {
            return Err(Error::Corrupt);
        }
        let opened = Arc::new(OpenedObject {
            plain_len: opener.header().plaintext_len(),
            wrapped: opener.header().wrapped().clone(),
            opener,
        });
        lock(&self.opened).insert(cache_key, opened.clone(), 0);
        Ok(opened)
    }
}

impl<B: ObjectBackend> ObjectBackend for SealedBackend<B> {
    fn head<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
    ) -> Operation<'a, ObjectInfo> {
        Box::pin(async move {
            let info = self.inner.head(context, key).await?;
            let opened = self.open(context, key, &info.version, info.length).await?;
            Ok(ObjectInfo {
                length: opened.plain_len,
                ..info
            })
        })
    }

    fn create<'a>(
        &'a self,
        context: &'a RequestContext,
        request: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            let sealed = self.seal(request)?;
            let receipt = self.inner.create(context, &sealed.sealed).await?;
            Ok(WriteReceipt {
                request_id: receipt.request_id,
                object: ObjectInfo {
                    length: sealed.plain_len,
                    ..receipt.object
                },
            })
        })
    }

    fn read_range<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
        version: &'a VersionToken,
        range: ByteRange,
    ) -> Operation<'a, Payload> {
        Box::pin(async move {
            if range.length() > MAX_PAYLOAD {
                return Err(Error::InvalidInput);
            }
            let cached = lock(&self.opened).get(&(key.clone(), version.clone()));
            let opened = match cached {
                Some(found) => {
                    let scope = self.scopes.scope(key)?;
                    self.recheck(key, &scope, &found)?;
                    found
                }
                None => {
                    let info = self.inner.head(context, key).await?;
                    if &info.version != version {
                        return Err(Error::NotFound);
                    }
                    self.open(context, key, version, info.length).await?
                }
            };
            range.validate(opened.plain_len)?;
            let size = u64::from(opened.opener.header().block_size());
            let first = range.offset() / size;
            let last = (range.offset() + range.length() - 1) / size;
            let per_read = (MAX_PAYLOAD / (size + TAG_LEN as u64)).max(1);
            let mut out = Vec::with_capacity(usize::try_from(range.length()).unwrap_or(0));
            let mut from = first;
            while from <= last {
                let to = last.min(from + per_read - 1);
                let start = opened
                    .opener
                    .block_range(from)
                    .map_err(|_| Error::Corrupt)?
                    .start;
                let end = opened
                    .opener
                    .block_range(to)
                    .map_err(|_| Error::Corrupt)?
                    .end;
                let cipher = self
                    .inner
                    .read_range(context, key, version, ByteRange::new(start, end - start)?)
                    .await?;
                for ordinal in from..=to {
                    let r = opened
                        .opener
                        .block_range(ordinal)
                        .map_err(|_| Error::Corrupt)?;
                    let lo = usize::try_from(r.start - start).map_err(|_| Error::Corrupt)?;
                    let hi = usize::try_from(r.end - start).map_err(|_| Error::Corrupt)?;
                    let block = cipher.as_bytes().get(lo..hi).ok_or(Error::Corrupt)?;
                    let plain = opened
                        .opener
                        .open_block(ordinal, block)
                        .map_err(|_| Error::Corrupt)?;
                    let block_start = ordinal * size;
                    let take_from = range.offset().max(block_start) - block_start;
                    let take_to = (range.offset() + range.length())
                        .min(block_start + plain.len() as u64)
                        - block_start;
                    out.extend_from_slice(
                        plain
                            .get(take_from as usize..take_to as usize)
                            .ok_or(Error::Corrupt)?,
                    );
                }
                from = to + 1;
            }
            Payload::new(&out)
        })
    }
}
