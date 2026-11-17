//! The object-durable search store. This module set grows with M06; it starts with publishing a
//! base: the files of one native commit, the manifest pages and root that name them, and the
//! structural-head transition that makes the root the recovery start.
pub mod publish;
pub mod recover;
