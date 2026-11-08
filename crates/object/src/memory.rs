//! Bounded, nonpersistent memory and fault-injecting reference backend.
//!
//! This is a conformance double, not a durable or provider implementation. All
//! old versions and accepted request identities are retained until the backend
//! is dropped. Exhaustion refuses new mutations rather than evicting identity
//! evidence or silently changing exact-version reads. No background work runs.

use crate::{ByteRange, Error, MetadataStore, Mutation, ObjectBackend, ObjectInfo,
    ObjectKey, Operation, Payload, Precondition, RequestContext, RequestId,
    VersionToken, WriteReceipt};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, atomic::{AtomicU64, Ordering}};

static NEXT_BACKEND: AtomicU64 = AtomicU64::new(1);

/// Finite retained data limits. Bounds include all historical object versions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryLimits {
    /// Distinct keys retained, at most 4096.
    pub objects: u32,
    /// Immutable versions retained, at most 16384.
    pub versions: u32,
    /// Bytes of version keys, payloads and metadata, at most 64 MiB.
    pub version_bytes: u64,
    /// Accepted request identities retained, at most 16384.
    pub identities: u32,
    /// Retained request and receipt bytes, at most 64 MiB.
    pub identity_bytes: u64,
}

impl MemoryLimits {
    fn validate(self) -> Result<Self, Error> {
        if self.objects == 0 || self.objects > 4096 || self.versions == 0 || self.versions > 16384
            || self.identities == 0 || self.identities > 16384 || self.version_bytes == 0
            || self.version_bytes > 64 * 1024 * 1024 || self.identity_bytes == 0
            || self.identity_bytes > 64 * 1024 * 1024 {
            return Err(Error::InvalidInput);
        }
        Ok(self)
    }
}

/// One bounded deterministic mutation fault, consumed by a new mutation attempt.
///
/// Replaying an already accepted identity does not consume another fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    /// Return unavailable without accepting a write.
    UnavailableBeforeAcceptance,
    /// Cancel the context before the write admission check.
    CancelBeforeAcceptance,
    /// Expire the context before the write admission check.
    ExpireBeforeAcceptance,
    /// Commit the write, then lose its response.
    LoseResponseAfterCommit,
    /// Commit the write, then cancel the context and return unknown.
    CancelAfterCommit,
    /// Commit the write, then expire the context and return unknown.
    ExpireAfterCommit,
}

struct Stored {
    key: ObjectKey,
    info: ObjectInfo,
    payload: Payload,
}

struct Accepted {
    request: Mutation,
    receipt: WriteReceipt,
}

#[derive(Default)]
struct State {
    current: BTreeMap<ObjectKey, VersionToken>,
    versions: BTreeMap<VersionToken, Stored>,
    accepted: BTreeMap<RequestId, Accepted>,
    version_bytes: u64,
    identity_bytes: u64,
    next_version: u64,
    faults: VecDeque<Fault>,
}

/// Thread-safe memory double with atomic CAS and explicit bounded fault injection.
pub struct MemoryStore {
    backend: u64,
    limits: MemoryLimits,
    state: Mutex<State>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore").field("limits", &self.limits).finish_non_exhaustive()
    }
}

fn precondition_bytes(request: &Mutation) -> u64 {
    match &request.precondition {
        Precondition::Absent => 1,
        Precondition::Matches(token) => 1 + token.len() as u64,
    }
}

fn request_bytes(request: &Mutation) -> u64 {
    request.request_id.len() as u64 + request.key.len() as u64 + request.payload.len() as u64
        + request.metadata.len() as u64 + precondition_bytes(request)
}

fn info_bytes(info: &ObjectInfo) -> u64 {
    info.version.len() as u64 + 8 + info.metadata.len() as u64
}

fn receipt_bytes(receipt: &WriteReceipt) -> u64 {
    receipt.request_id.len() as u64 + info_bytes(&receipt.object)
}

impl MemoryStore {
    /// Create an empty nonpersistent backend with checked finite limits.
    pub fn new(limits: MemoryLimits) -> Result<Self, Error> {
        let limits = limits.validate()?;
        let backend = NEXT_BACKEND.fetch_update(Ordering::Relaxed, Ordering::Relaxed,
            |value| value.checked_add(1)).map_err(|_| Error::BudgetExceeded)?;
        Ok(Self { backend, limits, state: Mutex::new(State::default()) })
    }

    /// Queue one fault, refusing more than 32 pending faults before allocation.
    pub fn inject(&self, fault: Fault) -> Result<(), Error> {
        let mut state = self.state.lock().map_err(|_| Error::Unavailable)?;
        if state.faults.len() >= 32 {
            return Err(Error::BudgetExceeded);
        }
        state.faults.push_back(fault);
        Ok(())
    }

    fn replay<'a>(state: &'a State, request: &Mutation) -> Result<Option<&'a WriteReceipt>, Error> {
        match state.accepted.get(&request.request_id) {
            Some(accepted) if accepted.request != *request => Err(Error::IdentityConflict),
            Some(accepted) => Ok(Some(&accepted.receipt)),
            None => Ok(None),
        }
    }

    fn write(&self, context: &RequestContext, request: &Mutation) -> Result<WriteReceipt, Error> {
        context.charge_request(request_bytes(request))?;
        let mut state = self.state.lock().map_err(|_| Error::Unavailable)?;
        context.check()?;
        // Replay precedes precondition evaluation: a successful create's retry
        // returns its original receipt despite the key now being occupied.
        if let Some(receipt) = Self::replay(&state, request)? {
            context.charge_response(receipt_bytes(receipt))?;
            return Ok(receipt.clone());
        }
        let fault = state.faults.pop_front();
        match fault {
            Some(Fault::UnavailableBeforeAcceptance) => return Err(Error::Unavailable),
            Some(Fault::CancelBeforeAcceptance) => context.cancel(),
            Some(Fault::ExpireBeforeAcceptance) => context.expire(),
            _ => {}
        }
        context.check()?;
        match (&request.precondition, state.current.get(&request.key)) {
            (Precondition::Absent, None) => {}
            (Precondition::Matches(expected), Some(actual)) if expected == actual => {}
            _ => return Err(Error::Conflict),
        }
        let new_key = !state.current.contains_key(&request.key);
        let version_bytes = state.version_bytes.checked_add(request.key.len() as u64
            + request.payload.len() as u64 + request.metadata.len() as u64).ok_or(Error::BudgetExceeded)?;
        let next_version = state.next_version.checked_add(1).ok_or(Error::BudgetExceeded)?;
        let response_bytes = request.request_id.len() as u64 + 16 + 8 + request.metadata.len() as u64;
        let identity_bytes = state.identity_bytes.checked_add(request_bytes(request)
            + response_bytes).ok_or(Error::BudgetExceeded)?;
        if (new_key && state.current.len() >= self.limits.objects as usize)
            || state.versions.len() >= self.limits.versions as usize
            || state.accepted.len() >= self.limits.identities as usize
            || version_bytes > self.limits.version_bytes || identity_bytes > self.limits.identity_bytes {
            return Err(Error::BudgetExceeded);
        }
        // Reserve the complete known response before accepting. No fallible
        // budget check after commit can masquerade as a refused mutation.
        context.charge_response(response_bytes)?;
        context.check()?;
        let mut raw = [0; 16];
        raw[..8].copy_from_slice(&self.backend.to_be_bytes());
        raw[8..].copy_from_slice(&next_version.to_be_bytes());
        let info = ObjectInfo { version: VersionToken::new(&raw)?, length: request.payload.len() as u64,
            metadata: request.metadata.clone() };
        let receipt = WriteReceipt { request_id: request.request_id.clone(), object: info };
        state.current.insert(request.key.clone(), receipt.object.version.clone());
        state.versions.insert(receipt.object.version.clone(), Stored { key: request.key.clone(),
            info: receipt.object.clone(), payload: request.payload.clone() });
        state.accepted.insert(request.request_id.clone(), Accepted { request: request.clone(), receipt: receipt.clone() });
        state.version_bytes = version_bytes;
        state.identity_bytes = identity_bytes;
        state.next_version = next_version;
        match fault {
            Some(Fault::CancelAfterCommit) => { context.cancel(); Err(Error::UnknownOutcome) }
            Some(Fault::ExpireAfterCommit) => { context.expire(); Err(Error::UnknownOutcome) }
            Some(Fault::LoseResponseAfterCommit) => Err(Error::UnknownOutcome),
            _ => Ok(receipt),
        }
    }
}

impl ObjectBackend for MemoryStore {
    fn head<'a>(&'a self, context: &'a RequestContext, key: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        Box::pin(async move {
            context.charge_request(key.len() as u64)?;
            let state = self.state.lock().map_err(|_| Error::Unavailable)?;
            context.check()?;
            let version = state.current.get(key).ok_or(Error::NotFound)?;
            let stored = state.versions.get(version).ok_or(Error::Corrupt)?;
            context.charge_response(info_bytes(&stored.info))?;
            Ok(stored.info.clone())
        })
    }

    fn create<'a>(&'a self, context: &'a RequestContext, request: &'a Mutation) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            if request.precondition != Precondition::Absent {
                return Err(Error::InvalidInput);
            }
            self.write(context, request)
        })
    }

    fn read_range<'a>(&'a self, context: &'a RequestContext, key: &'a ObjectKey,
        version: &'a VersionToken, range: ByteRange) -> Operation<'a, Payload> {
        Box::pin(async move {
            context.charge_request(key.len() as u64 + version.len() as u64 + 16)?;
            let state = self.state.lock().map_err(|_| Error::Unavailable)?;
            context.check()?;
            let stored = state.versions.get(version).filter(|value| value.key == *key).ok_or(Error::NotFound)?;
            range.validate(stored.info.length)?;
            context.charge_response(range.length())?;
            let start = usize::try_from(range.offset()).map_err(|_| Error::InvalidRange)?;
            let length = usize::try_from(range.length()).map_err(|_| Error::InvalidRange)?;
            Payload::new(&stored.payload.as_bytes()[start..start + length])
        })
    }
}

impl MetadataStore for MemoryStore {
    fn compare_exchange<'a>(&'a self, context: &'a RequestContext, request: &'a Mutation) -> Operation<'a, WriteReceipt> {
        Box::pin(async move { self.write(context, request) })
    }

    fn resolve<'a>(&'a self, context: &'a RequestContext, request: &'a Mutation) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            context.charge_request(request_bytes(request))?;
            let state = self.state.lock().map_err(|_| Error::Unavailable)?;
            context.check()?;
            let receipt = Self::replay(&state, request)?.ok_or(Error::ResolutionUnavailable)?;
            context.charge_response(receipt_bytes(receipt))?;
            Ok(receipt.clone())
        })
    }
}
