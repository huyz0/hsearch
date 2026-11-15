//! Single-node write path v0.
//!
//! One writer per index. A bulk becomes a log slot created in the object store
//! under a derived key with a conditional create; it is acknowledged only after
//! that succeeds. The writer applies the slot to a scratch engine, can publish
//! the engine's segments and a root by conditional update, and rebuilds itself
//! from the store alone by replaying the log, with no LIST on any path.
//!
//! The v0 formats are disposable. Out of scope, by design: fencing across
//! concurrent owners beyond refusing a stale root, multi-node routing,
//! authority barriers, the coalesced refresh and visibility target (M06.07),
//! segment-based writer recovery (the engine reopens only from an in-process
//! checkpoint today, so a restarted writer replays the whole log) and sharing a
//! log between indexes (M03.09).
mod error;
pub mod format;
mod index;
mod store;
mod writer;

pub use error::{Error, Result};
pub use format::{
    BLOB_CHUNK_BYTES, Blob, BlobAt, Field, INLINE_FIELD_MAX, MAX_INDEXED_DOCUMENT_BYTES,
    MAX_SLOT_TEXT_BYTES, Op,
};
pub use index::{Index, get_mapping, get_shared, open_published, put_mapping, put_shared};
pub use writer::{Hooks, ItemResult, NoHooks, Point, Published, Wait};
