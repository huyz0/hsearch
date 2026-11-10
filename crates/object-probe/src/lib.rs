//! Bounded source-only probe components, not provider or readiness evidence.
//!
//! The library has no network or production persistence format. The fixed
//! diagnostic executable and Python capture path require owner authentication
//! and separately integrated ordinary TaskStore/coverage qualification.
#![forbid(unsafe_code)]

pub mod checker;
pub mod codec;
pub mod model;

/// Complete required capability eligibility.
pub mod eligibility;
/// Upload/part/completion model.
pub mod multipart;
/// Same-volume restart model.
pub mod restart;
