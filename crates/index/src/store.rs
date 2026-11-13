//! Blocking drivers and key derivation over the object-store traits. Keys are
//! derived from the index name and a position, never discovered by LIST.
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use hsearch_engine::{RemoteError, RemoteFiles};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error as StoreError, Metadata, MetadataStore, Mutation,
    ObjectKey, Payload, Precondition, RequestContext, RequestId, VersionToken, WriteReceipt,
};

use crate::{Error, Result};

struct Unpark(std::thread::Thread);
impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drive one store future to completion on this thread.
pub(crate) fn block<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::park();
    }
}

/// One fresh context per workflow step: finite time, bytes and attempts.
pub(crate) fn context() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: 64 << 20,
            response_bytes: 64 << 20,
            attempts: 64,
        },
    )
    .expect("a constant context is valid")
}

/// An index name: one short path component, so keys cannot escape the prefix.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

pub(crate) fn key(text: &str) -> Result<ObjectKey> {
    ObjectKey::new(text.as_bytes()).map_err(Error::from)
}

pub(crate) fn root_key(index: &str) -> String {
    format!("{index}/root")
}

pub(crate) fn slot_key(index: &str, seq: u64) -> String {
    format!("{index}/log/{seq:020}")
}

pub(crate) fn segment_key(index: &str, epoch: u64, name: &str) -> String {
    format!("{index}/seg/{epoch:020}/{name}")
}

pub(crate) fn mutation(
    request: &str,
    key: &str,
    payload: &[u8],
    precondition: Precondition,
) -> Result<Mutation> {
    Ok(Mutation {
        request_id: RequestId::new(request.as_bytes()).map_err(Error::from)?,
        key: self::key(key)?,
        payload: Payload::new(payload).map_err(Error::from)?,
        metadata: Metadata::new(b"").map_err(Error::from)?,
        precondition,
    })
}

/// How a conditional write ended, with the unknown case resolved if possible.
pub(crate) enum Written {
    /// The store holds exactly this mutation.
    Done(WriteReceipt),
    /// The key is held by something else: another writer.
    Occupied,
    /// The store certainly did not accept it; retrying the same request is safe.
    Refused(StoreError),
    /// Whether it was accepted could not be established.
    Unknown,
}

/// Create (`replace` false) or compare-exchange one object, resolving a lost
/// response with the original identity, and retrying refusals a few times.
pub(crate) fn write<S: MetadataStore>(store: &S, request: &Mutation, replace: bool) -> Written {
    let mut last = StoreError::Unavailable;
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50 << attempt));
        }
        let ctx = context();
        let result = if replace {
            block(store.compare_exchange(&ctx, request))
        } else {
            block(store.create(&ctx, request))
        };
        match result {
            Ok(receipt) => return Written::Done(receipt),
            Err(StoreError::Conflict) => {
                // A retry of our own accepted create also reports occupied on
                // some providers: ask by identity before deciding.
                return match block(store.resolve(&context(), request)) {
                    Ok(receipt) => Written::Done(receipt),
                    // Our identity is not on record: the key is someone else's. Poisoning
                    // on this is the safe direction; nothing is acknowledged either way.
                    Err(
                        StoreError::NotFound
                        | StoreError::IdentityConflict
                        | StoreError::ResolutionUnavailable,
                    ) => Written::Occupied,
                    Err(_) => Written::Unknown,
                };
            }
            Err(StoreError::UnknownOutcome | StoreError::DeadlineExceeded) => {
                return match block(store.resolve(&context(), request)) {
                    Ok(receipt) => Written::Done(receipt),
                    Err(_) => Written::Unknown,
                };
            }
            Err(error @ (StoreError::Unavailable | StoreError::Throttled)) => last = error,
            Err(error) => return Written::Refused(error),
        }
    }
    Written::Refused(last)
}

/// Read a whole object at its latest version, in bounded ranges.
pub(crate) fn read_all<S: MetadataStore>(store: &S, key: &ObjectKey) -> Result<Vec<u8>> {
    let info = block(store.head(&context(), key))?;
    read_version(store, key, &info.version, info.length)
}

pub(crate) fn read_version<S: MetadataStore>(
    store: &S,
    key: &ObjectKey,
    version: &VersionToken,
    length: u64,
) -> Result<Vec<u8>> {
    const STEP: u64 = 8 << 20;
    if length > (1 << 30) {
        return Err(Error::Corrupt);
    }
    let mut out = Vec::with_capacity(usize::try_from(length).map_err(|_| Error::Corrupt)?);
    let mut offset = 0;
    while offset < length {
        let take = STEP.min(length - offset);
        let range = ByteRange::new(offset, take).map_err(Error::from)?;
        let part = block(store.read_range(&context(), key, version, range))?;
        out.extend_from_slice(part.as_bytes());
        offset += take;
    }
    Ok(out)
}

/// Whole-file fetches of published segment files, by manifest key.
pub(crate) struct StoreFiles<S> {
    pub(crate) store: Arc<S>,
    pub(crate) keys: std::collections::BTreeMap<String, String>,
}

impl<S: MetadataStore + 'static> RemoteFiles for StoreFiles<S> {
    fn fetch(&self, name: &str) -> std::result::Result<Vec<u8>, RemoteError> {
        let key = self.keys.get(name).ok_or(RemoteError::NotFound)?;
        let key = ObjectKey::new(key.as_bytes()).map_err(|_| RemoteError::NotFound)?;
        read_all(&*self.store, &key).map_err(|error| match error {
            Error::Store(StoreError::NotFound) => RemoteError::NotFound,
            _ => RemoteError::Unavailable,
        })
    }
}
