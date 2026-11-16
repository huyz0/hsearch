//! The key manager: resolves an index's key, creates and opens per-object data keys, and keeps KMS calls
//! off the per-request path with a bounded, zeroizing data-key cache.
//!
//! Cache policy, all of it stated here because a performance target must not be met by quietly keeping keys
//! longer than their policy allows:
//!
//! * Bounded: at most [`CachePolicy::max_entries`] keys, oldest inserted evicted first (insertion order is
//!   expiry order, apart from an entry re-inserted over itself, so this is nearly earliest-expiry first;
//!   expired entries at the front are also dropped on every insert). Memory is `max_entries * ~200 B`.
//! * Sharded: the cache is [`CachePolicy::shards`] independent locks, picked by the envelope digest, so
//!   reads of different objects do not contend. The cost is that the bound and the eviction order hold per
//!   shard: a shard can evict while another has room. `shards: 1` is one exact global order.
//! * Time-limited: an entry is dead after [`CachePolicy::ttl`] on the monotonic clock. There is no refresh
//!   on error and no stale fallback: an expired entry plus an unreachable provider is an error.
//! * Keyed by index, crypto domain, envelope digest (which covers key reference, provider key version and
//!   wrapped bytes) and policy generation, so a new generation never sees a key cached under the old one.
//! * Single flight: concurrent misses for one key make one provider call.
//! * Invalidated by index or by key, immediately, when a revocation is observed.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::audit::{AuditEvent, AuditOp, AuditSink};
use crate::clock::Clock;
use crate::ids::IndexId;
use crate::keys::{
    CallOptions, KeyContext, KeyProvider, KeyRef, KeyVersion, Profile, ProviderError, SecretKey,
    WrappedKey,
};

/// Why a key operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyError {
    /// The provider failed.
    Provider(ProviderError),
    /// The key's provider or account is not on the operator's allowlist.
    NotAllowed,
    /// The key exists but is not usable (revoked or disabled).
    KeyUnusable,
    /// A test provider was given to a production profile.
    TestProviderInProduction,
    /// The key belongs to a different provider than this manager talks to.
    WrongProvider,
    /// The cache policy is unusable (no entries, no lifetime, or more shards than entries).
    BadCachePolicy,
    /// The deployment default key serves every index that names none; destroying it is not one index's
    /// crypto-shredding and is refused here.
    DefaultKeyProtected,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Provider(e) => write!(f, "{e}"),
            KeyError::NotAllowed => f.write_str("key is not on the operator allowlist"),
            KeyError::KeyUnusable => f.write_str("key is not usable"),
            KeyError::TestProviderInProduction => {
                f.write_str("a test key provider is refused in the production profile")
            }
            KeyError::WrongProvider => f.write_str("key belongs to another provider"),
            KeyError::DefaultKeyProtected => {
                f.write_str("the deployment default key cannot be shredded for one index")
            }
            KeyError::BadCachePolicy => f.write_str(
                "cache policy needs entries, a non-zero lifetime and 1..=max_entries shards",
            ),
        }
    }
}

impl std::error::Error for KeyError {}

impl From<ProviderError> for KeyError {
    fn from(e: ProviderError) -> Self {
        KeyError::Provider(e)
    }
}

/// Operator-configured providers and accounts a key reference may name. A tenant cannot widen it.
#[derive(Clone, Debug)]
pub struct KeyAllowlist {
    entries: Vec<(String, String)>,
}

impl KeyAllowlist {
    /// An allowlist of `(provider, account)` pairs.
    pub fn new(entries: Vec<(String, String)>) -> Self {
        Self { entries }
    }

    /// Whether `key` names an allowed provider and account.
    pub fn allows(&self, key: &KeyRef) -> bool {
        self.entries
            .iter()
            .any(|(p, a)| p == key.provider() && a == key.account())
    }
}

/// The data-key cache's bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CachePolicy {
    /// Most keys held.
    pub max_entries: usize,
    /// How long a key may be used after it was unwrapped.
    pub ttl: Duration,
    /// How many independent locks the cache is split over (at most `max_entries`). 1 gives one exact
    /// global eviction order; more removes read contention between different objects.
    pub shards: usize,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            ttl: Duration::from_secs(300),
            shards: 16,
        }
    }
}

/// Counters, for metrics and for asserting the call budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups answered from the cache.
    pub hits: u64,
    /// Lookups that were not.
    pub misses: u64,
    /// Misses that waited for another caller's provider call.
    pub coalesced: u64,
    /// Keys evicted to stay within the bound.
    pub evictions: u64,
    /// Keys dropped by invalidation.
    pub invalidated: u64,
}

#[derive(Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    coalesced: AtomicU64,
    evictions: AtomicU64,
    invalidated: AtomicU64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    index: IndexId,
    domain: [u8; 16],
    object: [u8; 16],
    digest: [u8; 32],
    generation: u64,
}

/// A cached key and what it is scoped to. The map is keyed by the envelope digest alone, and a lookup also
/// compares this scope, so a hit needs no allocation and no reference-count traffic on shared values: the
/// scope is compared in place, not cloned into a composite key.
struct Slot {
    dek: Arc<SecretKey>,
    expires: Duration,
    key: KeyRef,
    index: IndexId,
    domain: [u8; 16],
    object: [u8; 16],
    generation: u64,
}

#[derive(Default)]
struct CacheState {
    map: HashMap<[u8; 32], Slot>,
    order: VecDeque<[u8; 32]>,
}

struct Shard {
    cap: usize,
    state: RwLock<CacheState>,
}

type Outcome = Result<Arc<SecretKey>, ProviderError>;

struct Flight {
    result: Mutex<Option<Outcome>>,
    done: Condvar,
}

/// Resolves keys and manages data keys for a deployment.
pub struct KeyManager {
    provider: Arc<dyn KeyProvider>,
    allow: KeyAllowlist,
    default_key: KeyRef,
    policy: CachePolicy,
    audit: Arc<dyn AuditSink>,
    clock: Arc<dyn Clock>,
    shards: Box<[Shard]>,
    inflight: Mutex<HashMap<CacheKey, Arc<Flight>>>,
    /// Bumped by every invalidation, before it drops anything.
    epoch: AtomicU64,
    counters: Counters,
}

impl KeyManager {
    /// A manager over `provider`. A production profile refuses a test provider, and the default key must be
    /// on the allowlist and the provider's own.
    pub fn new(
        profile: Profile,
        provider: Arc<dyn KeyProvider>,
        allow: KeyAllowlist,
        default_key: KeyRef,
        policy: CachePolicy,
        audit: Arc<dyn AuditSink>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, KeyError> {
        if profile == Profile::Production && provider.is_test() {
            return Err(KeyError::TestProviderInProduction);
        }
        if policy.max_entries == 0
            || policy.ttl.is_zero()
            || policy.shards == 0
            || policy.shards > policy.max_entries
        {
            return Err(KeyError::BadCachePolicy);
        }
        let manager = Self {
            provider,
            allow,
            default_key,
            policy,
            audit,
            clock,
            shards: (0..policy.shards)
                .map(|i| Shard {
                    cap: policy.max_entries / policy.shards
                        + usize::from(i < policy.max_entries % policy.shards),
                    state: RwLock::new(CacheState::default()),
                })
                .collect(),
            inflight: Mutex::new(HashMap::new()),
            epoch: AtomicU64::new(0),
            counters: Counters::default(),
        };
        manager.admit(&manager.default_key)?;
        Ok(manager)
    }

    fn admit(&self, key: &KeyRef) -> Result<(), KeyError> {
        if key.provider() != self.provider.name() {
            return Err(KeyError::WrongProvider);
        }
        if !self.allow.allows(key) {
            return Err(KeyError::NotAllowed);
        }
        Ok(())
    }

    fn record(
        &self,
        op: AuditOp,
        index: &IndexId,
        key: Option<&KeyRef>,
        version: Option<&KeyVersion>,
        generation: u64,
        outcome: &'static str,
    ) {
        self.audit.record(AuditEvent {
            at: self.clock.wall(),
            op,
            index: index.clone(),
            key: key.cloned(),
            version: version.cloned(),
            generation,
            outcome,
        });
    }

    /// The deployment default key, for indexes that name none.
    pub fn default_key(&self) -> &KeyRef {
        &self.default_key
    }

    /// The key an index uses: its own reference, or the deployment default.
    pub fn resolve<'a>(&'a self, index_key: Option<&'a KeyRef>) -> &'a KeyRef {
        index_key.unwrap_or(&self.default_key)
    }

    /// Checks that `key` may be assigned to `index`: on the allowlist, the provider's own, and usable now.
    /// Call it when a policy sets an index's key, not on the request path.
    pub fn validate_key(
        &self,
        index: &IndexId,
        key: &KeyRef,
        generation: u64,
        opts: &CallOptions,
    ) -> Result<KeyVersion, KeyError> {
        if let Err(e) = self.admit(key) {
            self.record(
                AuditOp::Refused,
                index,
                Some(key),
                None,
                generation,
                "not_allowed",
            );
            return Err(e);
        }
        match self.provider.key_info(key, opts) {
            Ok(info) if info.usable && info.account == key.account() => {
                self.record(
                    AuditOp::Activate,
                    index,
                    Some(key),
                    Some(&info.current),
                    generation,
                    "ok",
                );
                Ok(info.current)
            }
            Ok(_) => {
                self.record(
                    AuditOp::Activate,
                    index,
                    Some(key),
                    None,
                    generation,
                    "unusable",
                );
                Err(KeyError::KeyUnusable)
            }
            Err(e) => {
                self.record(
                    AuditOp::Activate,
                    index,
                    Some(key),
                    None,
                    generation,
                    e.code(),
                );
                Err(e.into())
            }
        }
    }

    /// Creates a fresh data key for one new immutable object, wrapped under the index's key. One provider
    /// call per object, never per block or record. The plaintext key is cached so the writer's own first
    /// read does not call the provider.
    pub fn create_object_key(
        &self,
        index_key: Option<&KeyRef>,
        ctx: &KeyContext,
        generation: u64,
        opts: &CallOptions,
    ) -> Result<(Arc<SecretKey>, WrappedKey), KeyError> {
        let key = self.resolve(index_key);
        self.admit(key).inspect_err(|_| {
            self.record(
                AuditOp::Refused,
                &ctx.index,
                Some(key),
                None,
                generation,
                "not_allowed",
            )
        })?;
        let epoch = self.epoch.load(Ordering::SeqCst);
        let dek = SecretKey::generate()?;
        let wrapped = match self.provider.wrap(key, ctx, &dek, opts) {
            Ok(w) => w,
            Err(e) => {
                self.record(
                    AuditOp::Wrap,
                    &ctx.index,
                    Some(key),
                    None,
                    generation,
                    e.code(),
                );
                return Err(e.into());
            }
        };
        self.record(
            AuditOp::Wrap,
            &ctx.index,
            Some(key),
            Some(wrapped.version()),
            generation,
            "ok",
        );
        let dek = Arc::new(dek);
        self.insert(
            CacheKey::new(ctx, &wrapped, generation),
            &dek,
            wrapped.key(),
            epoch,
        );
        Ok((dek, wrapped))
    }

    /// Opens a stored object's data key. A cache hit makes no provider call. The wrapped key is untrusted
    /// storage input: its key reference is checked against the allowlist before the provider sees it.
    pub fn open_object_key(
        &self,
        wrapped: &WrappedKey,
        ctx: &KeyContext,
        generation: u64,
        opts: &CallOptions,
    ) -> Result<Arc<SecretKey>, KeyError> {
        self.admit(wrapped.key()).inspect_err(|_| {
            self.record(
                AuditOp::Refused,
                &ctx.index,
                Some(wrapped.key()),
                Some(wrapped.version()),
                generation,
                "not_allowed",
            )
        })?;
        let cache_key = CacheKey::new(ctx, wrapped, generation);
        // Three tries only to absorb a leader that was cancelled by its own caller.
        for _ in 0..3 {
            if let Some(dek) = self.lookup(&cache_key) {
                self.counters.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(dek);
            }
            match self.fetch(&cache_key, wrapped, ctx, generation, opts) {
                Err(ProviderError::Cancelled) if !opts.cancel.is_cancelled() => continue,
                other => return other.map_err(KeyError::from),
            }
        }
        Err(KeyError::Provider(ProviderError::Unavailable))
    }

    fn lookup(&self, key: &CacheKey) -> Option<Arc<SecretKey>> {
        let now = self.clock.monotonic();
        let cache = self
            .shard(key)
            .state
            .read()
            .unwrap_or_else(|p| p.into_inner());
        cache
            .map
            .get(&key.digest)
            .filter(|slot| {
                now < slot.expires
                    && slot.generation == key.generation
                    && slot.domain == key.domain
                    && slot.object == key.object
                    && slot.index == key.index
            })
            .map(|slot| slot.dek.clone())
    }

    fn fetch(
        &self,
        cache_key: &CacheKey,
        wrapped: &WrappedKey,
        ctx: &KeyContext,
        generation: u64,
        opts: &CallOptions,
    ) -> Outcome {
        let (flight, leader) = {
            let mut inflight = self.inflight.lock().unwrap_or_else(|p| p.into_inner());
            // Another caller may have finished between our cache miss and taking this lock.
            if let Some(dek) = self.lookup(cache_key) {
                self.counters.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(dek);
            }
            match inflight.get(cache_key) {
                Some(f) => (f.clone(), false),
                None => {
                    let f = Arc::new(Flight {
                        result: Mutex::new(None),
                        done: Condvar::new(),
                    });
                    inflight.insert(cache_key.clone(), f.clone());
                    (f, true)
                }
            }
        };
        if !leader {
            self.counters.coalesced.fetch_add(1, Ordering::Relaxed);
            return self.wait(&flight, opts);
        }
        self.counters.misses.fetch_add(1, Ordering::Relaxed);
        // However this call ends, even by a panic in the provider or the audit sink, the flight is completed
        // and removed; otherwise every later reader of this key would wait on it until restart.
        let mut guard = FlightGuard {
            manager: self,
            key: cache_key,
            flight: &flight,
            done: false,
        };
        let epoch = self.epoch.load(Ordering::SeqCst);
        let outcome = self.provider.unwrap(wrapped, ctx, opts).map(Arc::new);
        match &outcome {
            Ok(dek) => {
                self.record(
                    AuditOp::Unwrap,
                    &ctx.index,
                    Some(wrapped.key()),
                    Some(wrapped.version()),
                    generation,
                    "ok",
                );
                self.insert(cache_key.clone(), dek, wrapped.key(), epoch);
            }
            Err(e) => {
                self.record(
                    AuditOp::Unwrap,
                    &ctx.index,
                    Some(wrapped.key()),
                    Some(wrapped.version()),
                    generation,
                    e.code(),
                );
                // Only the key owner's revocation flushes the index. A plain denial, which anyone who can
                // plant wrapped keys in storage could provoke, must not turn into a flush of every cached key.
                if matches!(e, ProviderError::Revoked) {
                    self.invalidate_index(&ctx.index, generation);
                }
            }
        }
        guard.finish(outcome.clone());
        outcome
    }

    fn wait(&self, flight: &Flight, opts: &CallOptions) -> Outcome {
        // Real time, not the injectable clock: the clock bounds key lifetimes, this bounds how long a caller waits.
        let deadline = Instant::now() + opts.timeout;
        let mut guard = flight.result.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(result) = guard.as_ref() {
                return result.clone();
            }
            if opts.cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(ProviderError::Timeout);
            }
            // Short slices so a cancellation is noticed; the leader's notify ends the wait early.
            let (g, _) = flight
                .done
                .wait_timeout(guard, Duration::from_millis(20))
                .unwrap_or_else(|p| p.into_inner());
            guard = g;
        }
    }

    /// Caches `dek` unless an invalidation ran since `epoch` was read (a key unwrapped while a revocation
    /// was being applied must not outlive it).
    fn insert(&self, key: CacheKey, dek: &Arc<SecretKey>, wrapper: &KeyRef, epoch: u64) {
        let slot = Slot {
            dek: dek.clone(),
            expires: self.clock.monotonic() + self.policy.ttl,
            key: wrapper.clone(),
            index: key.index.clone(),
            domain: key.domain,
            object: key.object,
            generation: key.generation,
        };
        let shard = self.shard(&key);
        let mut cache = shard.state.write().unwrap_or_else(|p| p.into_inner());
        if self.epoch.load(Ordering::SeqCst) != epoch {
            return;
        }
        if cache.map.insert(key.digest, slot).is_none() {
            cache.order.push_back(key.digest);
        }
        // Expired keys leave memory here instead of waiting for capacity pressure on an idle shard.
        let now = self.clock.monotonic();
        while let Some(front) = cache.order.front().copied() {
            match cache.map.get(&front) {
                None => {
                    cache.order.pop_front();
                }
                Some(slot) if slot.expires <= now => {
                    cache.order.pop_front();
                    cache.map.remove(&front);
                }
                Some(_) => break,
            }
        }
        while cache.map.len() > shard.cap {
            let Some(oldest) = cache.order.pop_front() else {
                break;
            };
            if cache.map.remove(&oldest).is_some() {
                self.counters.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Drops every cached data key of `index`, so the next use asks the provider (which refuses if the key
    /// is revoked). Other indexes' keys are untouched.
    pub fn invalidate_index(&self, index: &IndexId, generation: u64) -> usize {
        let n = self.drop_where(|s| &s.index == index);
        self.record(AuditOp::Invalidate, index, None, None, generation, "ok");
        n
    }

    /// Drops every cached data key wrapped by `key`, in every index: the owner revoked or rotated it.
    pub fn invalidate_key(
        &self,
        key: &KeyRef,
        index_for_audit: &IndexId,
        generation: u64,
    ) -> usize {
        let n = self.drop_where(|s| &s.key == key);
        self.record(
            AuditOp::Invalidate,
            index_for_audit,
            Some(key),
            None,
            generation,
            "ok",
        );
        n
    }

    /// Crypto-shredding: destroys the customer key at the provider, then drops every cached data key it
    /// wrapped. Other nodes' copies live until their cache lifetime or an applied revocation; the provider
    /// refuses every re-fetch. Returns how many cached keys were dropped here.
    /// dropped. If the provider cannot destroy the key, nothing is dropped and the refusal is audited, so a
    /// failed shred is visible and the keys keep working only as long as they would have anyway.
    pub fn shred_key(
        &self,
        key: &KeyRef,
        index: &IndexId,
        generation: u64,
        opts: &CallOptions,
    ) -> Result<usize, KeyError> {
        // Irreversible, so every refusal comes before the provider is touched, and an intent is audited
        // first: a crash between the provider call and the final record still leaves a trace.
        if key == &self.default_key {
            self.record(
                AuditOp::Refused,
                index,
                Some(key),
                None,
                generation,
                "default_key",
            );
            return Err(KeyError::DefaultKeyProtected);
        }
        self.admit(key).inspect_err(|_| {
            self.record(
                AuditOp::Refused,
                index,
                Some(key),
                None,
                generation,
                "not_allowed",
            )
        })?;
        self.record(
            AuditOp::Destroy,
            index,
            Some(key),
            None,
            generation,
            "requested",
        );
        match self.provider.destroy_key(key, opts) {
            Ok(()) => {
                let dropped = self.drop_where(|s| &s.key == key);
                self.record(AuditOp::Destroy, index, Some(key), None, generation, "ok");
                Ok(dropped)
            }
            Err(e) => {
                self.record(
                    AuditOp::Destroy,
                    index,
                    Some(key),
                    None,
                    generation,
                    e.code(),
                );
                Err(e.into())
            }
        }
    }

    /// Applies a revocation this node accepted: drops the index's cached data keys, so the next use asks
    /// the provider, and audits it. Returns how many were dropped.
    pub fn apply_revocation(&self, index: &IndexId, key: &KeyRef, generation: u64) -> usize {
        let dropped = self.drop_where(|s| &s.index == index);
        self.record(AuditOp::Revoke, index, Some(key), None, generation, "ok");
        dropped
    }

    /// Audits that `key` gained `version` at the provider. Rotation does not touch cached keys: wraps made
    /// under older versions keep unwrapping until their version is retired.
    pub fn note_rotation(
        &self,
        index: &IndexId,
        key: &KeyRef,
        version: &KeyVersion,
        generation: u64,
    ) {
        self.record(
            AuditOp::Rotate,
            index,
            Some(key),
            Some(version),
            generation,
            "ok",
        );
    }

    fn shard(&self, key: &CacheKey) -> &Shard {
        // The digest is SHA-256, so any eight bytes of it are uniform.
        let n = u64::from_le_bytes([
            key.digest[0],
            key.digest[1],
            key.digest[2],
            key.digest[3],
            key.digest[4],
            key.digest[5],
            key.digest[6],
            key.digest[7],
        ]);
        &self.shards[(n % self.shards.len() as u64) as usize]
    }

    fn drop_where(&self, doomed: impl Fn(&Slot) -> bool) -> usize {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let mut dropped = 0;
        for shard in &self.shards {
            let mut cache = shard.state.write().unwrap_or_else(|p| p.into_inner());
            let before = cache.map.len();
            cache.map.retain(|_, s| !doomed(s));
            let state = &mut *cache;
            state.order.retain(|k| state.map.contains_key(k));
            dropped += before - state.map.len();
        }
        self.counters
            .invalidated
            .fetch_add(dropped as u64, Ordering::Relaxed);
        dropped
    }

    /// Keys currently cached.
    pub fn cached(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.state.read().unwrap_or_else(|p| p.into_inner()).map.len())
            .sum()
    }

    /// A snapshot of the counters.
    pub fn stats(&self) -> CacheStats {
        let c = &self.counters;
        CacheStats {
            hits: c.hits.load(Ordering::Relaxed),
            misses: c.misses.load(Ordering::Relaxed),
            coalesced: c.coalesced.load(Ordering::Relaxed),
            evictions: c.evictions.load(Ordering::Relaxed),
            invalidated: c.invalidated.load(Ordering::Relaxed),
        }
    }
}

impl CacheKey {
    fn new(ctx: &KeyContext, wrapped: &WrappedKey, generation: u64) -> Self {
        Self {
            index: ctx.index.clone(),
            domain: ctx.domain,
            object: ctx.object,
            digest: *wrapped.digest(),
            generation,
        }
    }
}

struct FlightGuard<'a> {
    manager: &'a KeyManager,
    key: &'a CacheKey,
    flight: &'a Flight,
    done: bool,
}

impl FlightGuard<'_> {
    fn finish(&mut self, outcome: Outcome) {
        self.done = true;
        *self.flight.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(outcome);
        self.manager
            .inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(self.key);
        self.flight.done.notify_all();
    }
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.finish(Err(ProviderError::Unavailable));
        }
    }
}
