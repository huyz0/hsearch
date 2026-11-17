//! Bounded materialized Lucene adapter. Native visibility and native commit do
//! not establish hsearch authority, quorum durability, or archival state.
mod aggregations;
mod budget;
pub use budget::{DiskBudget, DiskReservation};
mod integrity;
pub use integrity::{verify_directory, verify_files};
mod maintenance;
pub use maintenance::{MergeScheduler, MergeTicket};
mod checkpoint;
mod directory;
pub use directory::EngineDirectory;
mod local;
mod remote;
pub use local::{LocalFiles, with_local_engine};
pub use remote::{CachePin, CachedDirectory, RemoteError, RemoteFiles};
mod range;
pub use range::{
    DEFAULT_DISK_BUDGET, DEFAULT_HEAP_BUDGET, DEFAULT_SPILL_BUDGET, ManifestEntry, ObjectBlobStore,
    RangeCacheMetrics, RangeCacheSettings, cache_namespace,
};
mod upload;
pub use upload::ObjectUploadSink;
mod schema;
pub use aggregations::{Bucket, Metrics};
mod adapter;
mod factory;
pub use factory::{
    Capability, EngineFactory, Feature, ShardEngine, Support, WriterMode, capability_of_mode,
    capability_of_query, capability_of_sort,
};
mod query;
mod reader;
mod vector;
pub use adapter::{Checkpoint, Engine, Mutation, Outcome, PreparedBatch, Receipt};
pub use query::Query;
pub use reader::{Hit, SearchResult, Snapshot, Sort, StoredValue};
pub use schema::{
    Kind, Limits, MAX_AUTO_MERGE_MB, MAX_DOCUMENT_BYTES, MAX_DOCUMENTS, MAX_FIELDS, MAX_OPERATIONS,
    MAX_TERMS, Schema, Token, Value, analyze_text,
};
pub use vector::{
    DEFAULT_IVF_CLUSTER_SIZE, IVF_CLUSTER_SIZE, Index, IvfQuantizer, MAX_DIMENSION,
    MAX_EF_CONSTRUCTION, MAX_KNN_CANDIDATES, MAX_KNN_K, MAX_M, MAX_NPROBE, MAX_VECTOR_FIELDS,
    Quantizer, Rerank, ScalarBits, Similarity, VectorSpec,
};

/// Exact upstream engine revision.
pub const ENGINE_REVISION: &str = "df02d694c4ffb96b2b765084b07728fa6a523a33";
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
