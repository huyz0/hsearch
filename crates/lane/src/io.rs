//! Drivers over the object-store traits: a bounded context per step, conditional writes with their
//! unknown outcome resolved, and whole-object reads. Nothing here lists a prefix.
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use hsearch_format::{CommitToken, Id};
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

/// Drive one future to completion on this thread. The lane API is `async` so a node can run it on
/// its own executor; tests, benches and tools use this.
pub fn block_on<T>(future: impl Future<Output = T>) -> T {
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

/// One fresh, finite context per workflow step.
pub(crate) fn context() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: 16 << 20,
            response_bytes: 16 << 20,
            attempts: 16,
        },
    )
    .expect("a constant context is valid")
}

pub(crate) fn key(text: &str) -> Result<ObjectKey> {
    ObjectKey::new(text.as_bytes()).map_err(Error::from)
}

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// A unique, never-zero 16-byte token for one commit (M03.11). Unique across processes with
/// overwhelming probability: a randomly keyed hash of the clock, a process counter and the address
/// of a stack local. It is an identity, not a secret.
pub(crate) fn fresh_token() -> CommitToken {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let state = RandomState::new();
    let mut bytes = [0u8; 16];
    for (half, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = state.build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u64(n);
        hasher.write_usize(half);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    if bytes == [0; 16] {
        bytes[0] = 1;
    }
    CommitToken::new(bytes).expect("a nonzero token is valid")
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A request identity unique to one attempt, derived from what it writes.
pub(crate) fn request_id(label: &str, id: &Id, position: u64, token: &CommitToken) -> String {
    format!(
        "{label}-{}-{position}-{}",
        hex(&id[..8]),
        hex(&token.as_bytes()[..8])
    )
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

/// How a conditional write ended, with the unknown case resolved where possible.
pub(crate) enum Written {
    /// The store holds exactly this mutation.
    Done(WriteReceipt),
    /// The key (or version) is held by something else: another writer.
    Occupied,
    /// The store certainly did not accept it; the same request may be repeated.
    Refused(StoreError),
    /// Whether it was accepted could not be established.
    Unknown,
}

/// Create (`replace` false) or compare-exchange one object once. No retry and no sleep: the adapter
/// owns retry policy, and a caller that wants to repeat does so on the same position. A conflict or
/// a lost reply is resolved by the original identity before anything is concluded.
pub(crate) async fn write<S: MetadataStore>(
    store: &S,
    request: &Mutation,
    replace: bool,
) -> Written {
    let ctx = context();
    let result = if replace {
        store.compare_exchange(&ctx, request).await
    } else {
        store.create(&ctx, request).await
    };
    match result {
        Ok(receipt) => Written::Done(receipt),
        // A retry of our own accepted create also reports occupied on some providers: ask by
        // identity before deciding.
        Err(StoreError::Conflict) => match store.resolve(&context(), request).await {
            Ok(receipt) => Written::Done(receipt),
            Err(
                StoreError::NotFound
                | StoreError::IdentityConflict
                | StoreError::ResolutionUnavailable,
            ) => Written::Occupied,
            Err(_) => Written::Unknown,
        },
        Err(StoreError::UnknownOutcome | StoreError::DeadlineExceeded) => {
            match store.resolve(&context(), request).await {
                Ok(receipt) => Written::Done(receipt),
                Err(_) => Written::Unknown,
            }
        }
        // Only errors that precede acceptance are a certain refusal. Anything else (corrupt
        // response, exhausted budget, a missing key on a write) may follow acceptance: resolve.
        Err(
            error @ (StoreError::Unavailable
            | StoreError::Throttled
            | StoreError::PermissionDenied
            | StoreError::InvalidInput
            | StoreError::Unsupported
            | StoreError::Cancelled),
        ) => Written::Refused(error),
        Err(_) => match store.resolve(&context(), request).await {
            Ok(receipt) => Written::Done(receipt),
            Err(_) => Written::Unknown,
        },
    }
}

/// Read a whole object at its latest version with one HEAD and one ranged GET, or `None` if the key
/// is absent. `bound` is the largest length accepted, checked before the read. The bytes are the
/// adapter's own buffer: not copied again here.
pub(crate) async fn read_object<S: MetadataStore>(
    store: &S,
    key: &ObjectKey,
    bound: u64,
) -> Result<Option<(Payload, VersionToken)>> {
    let info = match store.head(&context(), key).await {
        Ok(info) => info,
        Err(StoreError::NotFound) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if info.length > bound {
        return Err(Error::Format(hsearch_format::FormatError::TooLarge(
            "stored object",
        )));
    }
    if info.length == 0 {
        return Err(Error::Format(hsearch_format::FormatError::Truncated));
    }
    let range = ByteRange::new(0, info.length).map_err(Error::from)?;
    let part = store
        .read_range(&context(), key, &info.version, range)
        .await?;
    Ok(Some((part, info.version)))
}
