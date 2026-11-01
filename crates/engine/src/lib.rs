//! Lucene Rust dependency boundary. Remote input and search integration remain M02/M04 work.

/// The reviewed engine source revision. The dependency policy checks Cargo's actual lock.
pub const ENGINE_REVISION: &str = "46e280bc2a208544fb36ecd95932e89cd8bc5618";

pub use lucene_store::{Directory, Input};
