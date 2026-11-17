//! A single-process hsearch node: a small OpenSearch-shaped HTTP subset over the
//! object-store write path. Plain HTTP on loopback, no authentication.
//!
//! Implemented: `PUT /{index}` with explicit mappings, `POST /_bulk` and
//! `POST /{index}/_bulk` (index and delete with explicit ids), `GET
//! /{index}/_doc/{id}`, `POST /{index}/_search` and `POST /{index}/_refresh`.
//! Per-index `dynamic` is `true`, `false` or `strict` (default `strict`); `false`
//! ignores and `strict` rejects an undeclared field, and `true` is limit-checked
//! but cannot add fields until online mapping evolution (M21.04).
//! Not implemented, and refused rather than approximated: generated ids, `create` and `update` actions, scores (hits carry
//! `_score: null`), aggregations, scroll, real-time get (a get reads the latest
//! searchable view) and everything not listed.
mod bulk;
mod cli;
mod date;
mod error;
mod http;
mod limits;
mod mapping;
mod node;
mod policy;
mod search;
mod source_line;

pub use bulk::{MAX_BULK_BYTES, MAX_BULK_ITEMS};
pub use cli::{Options, parse_args};
pub use error::ApiError;
pub use http::{Running, serve};
pub use limits::{COST_FACTOR, Limits, MAX_DOCUMENT_CEILING, MIN_BUFFER_BYTES};
pub use mapping::Mapping;
pub use node::Node;
pub use policy::{Dynamic, MAX_FIELDS_CEILING, MappingLimits};
pub use search::MAX_WINDOW;
