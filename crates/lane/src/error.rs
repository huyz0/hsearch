//! One error type for lanes: never carries payloads, keys or provider text.
use hsearch_format::{FormatError, Id};
use hsearch_object::Error as StoreError;

/// Why a lane operation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The store certainly did not accept the operation (or a read failed). A write may be retried
    /// at the same position; nothing was acknowledged.
    Store(StoreError),
    /// Stored bytes are not a valid lane object, or are not the object their key named.
    Format(FormatError),
    /// The lane has no descriptor.
    NoLane,
    /// The lane has no structural head.
    NoHead,
    /// There is no catalog with this identity.
    NoCatalog,
    /// The catalog holds no live binding of this name to this identity.
    NoName,
    /// The lane (or group), head, catalog or live name already exists.
    Exists,
    /// A record names an index that is not a member of this lane.
    NotMember(Id),
    /// A record names an index sealed in this lane (promoted or left).
    Sealed(Id),
    /// Another writer holds this lane: our slot position was taken. The writer is poisoned.
    Fenced,
    /// The writer stopped after a conflict or a lost reply and must be reopened from the store.
    Poisoned,
    /// Whether the slot at this ordinal reached the store could not be established. It may appear
    /// after reopening; it was **not acknowledged**. The writer is poisoned.
    Unknown {
        /// The slot position that was being written.
        ordinal: u64,
    },
    /// The replay start is below the lane's durable reclaim cover: the position that read as the
    /// end of the log is a reclaimed hole, and sealing there would fork it. Reopen from the
    /// durable cover or above.
    BadCover,
    /// Sealing kept losing races to a live writer, or a descriptor change kept losing its CAS.
    Contended,
    /// The request is empty, over a bound, or a record the writer must not accept.
    Invalid(&'static str),
    /// A bounded structure is full (for example the placement cache).
    Capacity,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

impl From<StoreError> for Error {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<FormatError> for Error {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

/// Result alias for lane operations.
pub type Result<T> = std::result::Result<T, Error>;
