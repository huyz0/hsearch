//! Provider-independent bounded object and conditional metadata contracts.
//!
//! This crate defines no persisted encoding, commit authority, cloud provider or
//! durability claim. Version tokens are equality-only provider values. A lost
//! mutation response requires resolution using the original request identity.

mod context;
mod value;

pub use context::{Budget, Cancellation, RequestContext, Usage};
pub use value::{ByteRange, Metadata, Mutation, ObjectKey, Payload, RequestId, VersionToken};

use std::future::Future;
use std::pin::Pin;

/// One provider operation, dispatched once at the I/O boundary.
pub type Operation<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// A stable error code with no provider request, credential or payload text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The input is empty, malformed or exceeds an absolute contract bound.
    InvalidInput,
    /// The object or exact version does not exist.
    NotFound,
    /// Conditional creation or version comparison did not match.
    Conflict,
    /// The operation might have been accepted; resolve the same identity.
    UnknownOutcome,
    /// The provider is unavailable before accepting this operation.
    Unavailable,
    /// Access was denied before acceptance.
    PermissionDenied,
    /// The provider rejected admission due to throttling.
    Throttled,
    /// A required capability is absent.
    Unsupported,
    /// Stored data or a provider response violates the contract.
    Corrupt,
    /// Cancellation was observed before accepting a mutation.
    Cancelled,
    /// The deadline elapsed before accepting a mutation.
    DeadlineExceeded,
    /// A finite request, response, attempt or storage capacity was exhausted.
    BudgetExceeded,
    /// The range is empty, overflows, or exceeds the selected object's length.
    InvalidRange,
    /// An identity was reused for a different mutation.
    IdentityConflict,
    /// Retained identity resolution is no longer available; do not retry blindly.
    ResolutionUnavailable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

/// A conditional mutation. There is deliberately no unconditional overwrite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Precondition {
    /// Succeed only if this key has never been created in the current backend.
    Absent,
    /// Succeed only if the current object has exactly this opaque token.
    Matches(VersionToken),
}

/// Metadata returned without reading the payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectInfo {
    /// The exact immutable object version.
    pub version: VersionToken,
    /// The full payload length in bytes.
    pub length: u64,
    /// Opaque caller metadata, round-tripped without interpretation.
    pub metadata: Metadata,
}

/// A mutation result bound to the original caller identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteReceipt {
    /// Identity of the accepted request, unchanged by resolution or retry.
    pub request_id: RequestId,
    /// The accepted version and metadata.
    pub object: ObjectInfo,
}

/// Object reads bind the key and the exact version on every range operation.
///
/// Implementations must enforce context admission before allocation or I/O. The
/// returned range is complete or fails; a stale version never reads newer bytes.
/// Conditional create is linearizable and must not overwrite an existing key.
/// Provider adapters must expose unsupported capabilities explicitly.
pub trait ObjectBackend: Send + Sync {
    /// Read the latest metadata for this key within the request context.
    fn head<'a>(&'a self, context: &'a RequestContext, key: &'a ObjectKey)
        -> Operation<'a, ObjectInfo>;

    /// Create immutable content, requiring [`Precondition::Absent`].
    ///
    /// A different precondition returns [`Error::InvalidInput`] without a write.
    /// Cancellation or deadline after acceptance must return a receipt if known,
    /// or [`Error::UnknownOutcome`], never a claim that the mutation rolled back.
    fn create<'a>(&'a self, context: &'a RequestContext, request: &'a Mutation)
        -> Operation<'a, WriteReceipt>;

    /// Return exactly the requested bytes from this immutable version.
    fn read_range<'a>(&'a self, context: &'a RequestContext, key: &'a ObjectKey,
        version: &'a VersionToken, range: ByteRange) -> Operation<'a, Payload>;
}

/// Linearizable conditional metadata mutation and exact identity resolution.
///
/// A read followed by an unconditional write does not implement this trait.
/// Accepted retries with identical identity, key, body, metadata and precondition
/// resolve to the original receipt. Changed identity content is rejected.
/// Implementations advertise and enforce finite retention externally; an
/// unavailable resolution is not evidence that a mutation did not commit.
pub trait MetadataStore: ObjectBackend {
    /// Atomically create or replace only under the supplied precondition.
    fn compare_exchange<'a>(&'a self, context: &'a RequestContext, request: &'a Mutation)
        -> Operation<'a, WriteReceipt>;

    /// Resolve the full original mutation, detecting changed identity content.
    fn resolve<'a>(&'a self, context: &'a RequestContext, request: &'a Mutation)
        -> Operation<'a, WriteReceipt>;
}
