//! Bounded materialized Lucene adapter. Native visibility and native commit do
//! not establish hsearch authority, quorum durability, or archival state.
mod aggregations;
mod schema;
pub use aggregations::{Bucket, Metrics};
mod adapter;
mod query;
mod reader;
pub use adapter::{Checkpoint, Engine, Mutation, PreparedBatch, Receipt};
pub use query::Query;
pub use reader::{Hit, SearchResult, Snapshot, Sort, StoredValue};
pub use schema::{
    Kind, MAX_DOCUMENT_BYTES, MAX_DOCUMENTS, MAX_FIELDS, MAX_OPERATIONS, MAX_TERMS, Schema, Token,
    Value, analyze_text,
};

/// Exact upstream engine revision.
pub const ENGINE_REVISION: &str = "46e280bc2a208544fb36ecd95932e89cd8bc5618";
pub use lucene_store::{Directory, Input};

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
