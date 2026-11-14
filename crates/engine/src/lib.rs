//! Bounded materialized Lucene adapter. Native visibility and native commit do
//! not establish hsearch authority, quorum durability, or archival state.
mod aggregations;
mod checkpoint;
mod directory;
pub use directory::EngineDirectory;
mod local;
mod remote;
pub use local::{LocalFiles, with_local_engine};
pub use remote::{CachedDirectory, RemoteError, RemoteFiles};
mod schema;
pub use aggregations::{Bucket, Metrics};
mod adapter;
mod query;
mod reader;
pub use adapter::{Checkpoint, Engine, Mutation, Outcome, PreparedBatch, Receipt};
pub use query::Query;
pub use reader::{Hit, SearchResult, Snapshot, Sort, StoredValue};
pub use schema::{
    Kind, Limits, MAX_DOCUMENT_BYTES, MAX_DOCUMENTS, MAX_FIELDS, MAX_OPERATIONS, MAX_TERMS, Schema,
    Token, Value, analyze_text,
};

/// Exact upstream engine revision.
pub const ENGINE_REVISION: &str = "8c010db716b405f40a06f5d5573f1bd0270bfabf";
pub use lucene_store::{Directory, Input};

/// Native consuming cleanup outcome, without backend error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupStatus {
    /// Native cleanup returned success.
    Succeeded,
    /// Native cleanup returned an error.
    NativeFailure,
    /// The writer mutex was poisoned; its owner was still destroyed.
    Poisoned,
}
/// Actual acquired-lock release outcome, independent of writer cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseStatus {
    /// The wrapper explicitly delegated close and observed success.
    Acknowledged,
    /// The explicit backend close returned an error.
    Failed,
    /// Missing, stale, poisoned, duplicate or incomplete observation.
    Unobserved,
}

/// Redacted bounded adapter failures. Native error text never contains payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Invalid schema or unknown field.
    Schema,
    /// Wrong field type, missing field, or invalid identity/version.
    Document,
    /// A declared finite work or allocation bound was exceeded.
    Limit,
    /// A requested operation or combination is unsupported.
    Unsupported,
    /// Stable request identity or document version conflicts with prior state.
    Conflict,
    /// A native operation failed; mutation continuation may be poisoned.
    Native,
    /// Cleanup and actual lock release have separate bounded outcomes.
    Close {
        /// Result of consuming native writer cleanup.
        cleanup: CleanupStatus,
        /// Result for that writer's exact acquired lock.
        release: ReleaseStatus,
    },
    /// Native state cannot be proven to match the caller's checkpoint.
    Checkpoint,
    /// Concurrent work already holds the admission gate.
    Busy,
    /// Continuation after a possibly partial native mutation is refused.
    Poisoned,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "engine {self:?}")
    }
}
impl std::error::Error for Error {}
/// Adapter result with redacted typed errors.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
#[path = "../tests/support/adapter_directory.rs"]
mod adapter_test_support;
