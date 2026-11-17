//! The few object operations reclamation needs, with the request counts a run reports.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hsearch_lane::block_on;
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error as StoreError, Metadata, MetadataStore, Mutation,
    ObjectKey, Payload, Precondition, RequestContext, RequestId, VersionToken,
};
use sha2::{Digest as _, Sha256};

/// Largest object reclamation reads back (every object it reads is a root, page, pin or boundary).
pub(crate) const READ_BOUND: u64 = 1 << 20;

/// Requests a run made, by price class: reads (GET and HEAD), writes (PUT) and deletes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Requests {
    /// HEAD and ranged GET requests.
    pub reads: u64,
    /// PUT requests (create and compare-exchange).
    pub writes: u64,
    /// Version removals.
    pub deletes: u64,
    /// LIST pages, priced like a PUT.
    pub lists: u64,
}

/// Counters shared by one run's operations.
#[derive(Default)]
pub(crate) struct Counters {
    reads: AtomicU64,
    writes: AtomicU64,
    deletes: AtomicU64,
    lists: AtomicU64,
}

impl Counters {
    pub(crate) fn list(&self) {
        self.lists.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn snapshot(&self) -> Requests {
        Requests {
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            deletes: self.deletes.load(Ordering::Relaxed),
            lists: self.lists.load(Ordering::Relaxed),
        }
    }
}

/// A store plus a deadline and the counters of whoever is using it.
pub(crate) struct Io<S> {
    pub(crate) store: Arc<S>,
    deadline: Duration,
    pub(crate) counters: Counters,
}

/// A request identity that is a pure function of what it names, so a retry is the same request.
pub(crate) fn request_id(parts: &[&[u8]]) -> Result<RequestId, StoreError> {
    let mut h = Sha256::new();
    h.update(b"hs-reclaim\x01");
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    let d = h.finalize();
    let mut text = String::from("rc-");
    for b in &d[..16] {
        text.push_str(&format!("{b:02x}"));
    }
    RequestId::new(text.as_bytes())
}

impl<S: MetadataStore> Io<S> {
    pub(crate) fn new(store: Arc<S>, deadline: Duration) -> Self {
        Self {
            store,
            deadline,
            counters: Counters::default(),
        }
    }

    pub(crate) fn context(&self, body: usize) -> Result<RequestContext, StoreError> {
        RequestContext::new(
            self.deadline,
            Cancellation::default(),
            Budget {
                request_bytes: 4096 + body as u64,
                response_bytes: 4096 + READ_BOUND,
                attempts: 8,
            },
        )
    }

    /// The object at `key` and its version, or `None` when it does not exist.
    pub(crate) fn read(&self, key: &str) -> Result<Option<(Vec<u8>, VersionToken)>, StoreError> {
        let key = ObjectKey::new(key.as_bytes())?;
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        let info = match block_on(self.store.head(&self.context(0)?, &key)) {
            Ok(info) => info,
            Err(StoreError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        if info.length == 0 || info.length > READ_BOUND {
            return Err(StoreError::Corrupt);
        }
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        match block_on(self.store.read_range(
            &self.context(0)?,
            &key,
            &info.version,
            ByteRange::new(0, info.length)?,
        )) {
            Ok(part) => Ok(Some((part.as_bytes().to_vec(), info.version))),
            // Removed between the HEAD and the read: it is gone.
            Err(StoreError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether `key` exists.
    pub(crate) fn exists(&self, key: &str) -> Result<bool, StoreError> {
        let key = ObjectKey::new(key.as_bytes())?;
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        match block_on(self.store.head(&self.context(0)?, &key)) {
            Ok(_) => Ok(true),
            Err(StoreError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Write `bytes` under `precondition`; an unknown outcome is resolved by identity first.
    /// `Err(Conflict)` means the precondition did not hold.
    pub(crate) fn write(
        &self,
        key: &str,
        bytes: &[u8],
        precondition: Precondition,
        identity: RequestId,
    ) -> Result<(), StoreError> {
        let request = Mutation {
            request_id: identity,
            key: ObjectKey::new(key.as_bytes())?,
            payload: Payload::new(bytes)?,
            metadata: Metadata::new(b"")?,
            precondition: precondition.clone(),
        };
        self.counters.writes.fetch_add(1, Ordering::Relaxed);
        let ctx = self.context(bytes.len())?;
        let first = match precondition {
            Precondition::Absent => block_on(self.store.create(&ctx, &request)),
            Precondition::Matches(_) => block_on(self.store.compare_exchange(&ctx, &request)),
        };
        match first {
            Ok(_) => Ok(()),
            Err(StoreError::UnknownOutcome | StoreError::DeadlineExceeded) => {
                let ctx = self.context(0)?;
                block_on(self.store.resolve(&ctx, &request)).map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    /// Remove one version; a version already gone counts as removed.
    pub(crate) fn remove(&self, key: &str, version: &VersionToken) -> Result<(), StoreError> {
        let key = ObjectKey::new(key.as_bytes())?;
        self.counters.deletes.fetch_add(1, Ordering::Relaxed);
        match block_on(self.store.remove_version(&self.context(0)?, &key, version)) {
            Ok(()) | Err(StoreError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }
}
