//! The object-durable search store. This module set grows with M06; it starts with publishing a
//! base: the files of one native commit, the manifest pages and root that name them, and the
//! structural-head transition that makes the root the recovery start. Compaction, durable pins and
//! garbage collection reclaim what a replaced base left behind without losing a view anyone holds.
mod boundary;
pub mod compact;
pub mod cover;
pub mod host;
mod io;
pub mod log;
pub mod member;
pub mod payload;
pub mod pins;
pub mod pointer;
pub mod publish;
pub mod rebuild;
pub mod recover;
pub mod serve;
pub mod served;
pub mod shared;
pub mod snapshot;
pub mod sweep;
pub mod tail;

pub use io::Requests;
