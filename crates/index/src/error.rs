//! One error type for the write path, never carrying payloads or provider text.
use hsearch_object::Error as StoreError;

/// Why a write-path operation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The store refused or failed an operation. The write was not accepted.
    Store(StoreError),
    /// The engine refused or failed.
    Engine(hsearch_engine::Error),
    /// The input is empty, too large or malformed for a bulk.
    Invalid,
    /// The index has no room for this request.
    Capacity,
    /// The index does not exist.
    NotFound,
    /// The index already exists.
    Exists,
    /// Stored bytes are not a v0 object. Never guessed past.
    Corrupt,
    /// Whether the write reached the store could not be established. It may or
    /// may not appear after recovery; it was not acknowledged.
    Unknown,
    /// Another writer holds this index; this one must stop.
    Fenced,
    /// The writer stopped after a failure and must be reopened from the store.
    Poisoned,
    /// The write is durable but applying it failed; reopen to recover it.
    AppliedFailed,
    /// The index handle was closed.
    Closed,
    /// No searchable snapshot is available right now.
    Busy,
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

impl From<hsearch_engine::Error> for Error {
    fn from(error: hsearch_engine::Error) -> Self {
        Self::Engine(error)
    }
}

/// Result alias for the write path.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_display_and_convert_from_their_sources() {
        assert_eq!(Error::Fenced.to_string(), "Fenced");
        assert_eq!(
            Error::from(StoreError::Conflict),
            Error::Store(StoreError::Conflict)
        );
        assert_eq!(
            Error::from(hsearch_engine::Error::Limit),
            Error::Engine(hsearch_engine::Error::Limit)
        );
        let boxed: Box<dyn std::error::Error> = Box::new(Error::Corrupt);
        assert_eq!(boxed.to_string(), "Corrupt");
    }
}
