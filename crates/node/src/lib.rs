//! A single-process hsearch node: a small OpenSearch-shaped HTTP subset over the
//! object-store write path. Plain HTTP on loopback, no authentication.
//!
//! Implemented: `PUT /{index}` with explicit mappings, `POST /_bulk` and
//! `POST /{index}/_bulk` (index and delete with explicit ids), `GET
//! /{index}/_doc/{id}`, `POST /{index}/_search` and `POST /{index}/_refresh`.
//! Not implemented, and refused rather than approximated: dynamic mapping,
//! generated ids, `create` and `update` actions, scores (hits carry
//! `_score: null`), aggregations, scroll, real-time get (a get reads the latest
//! searchable view) and everything not listed.
mod bulk;
mod cli;
mod date;
mod error;
mod http;
mod mapping;
mod node;
mod search;

pub use bulk::{MAX_BULK_BYTES, MAX_BULK_ITEMS};
pub use cli::{Options, parse_args};
pub use error::ApiError;
pub use http::{Running, serve};
pub use mapping::Mapping;
pub use node::Node;
pub use search::MAX_WINDOW;
