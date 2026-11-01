//! Test-only helpers, excluded from production dependency closures.

/// Require thread-safe ownership at compile time for a public contract type.
pub fn assert_send_sync<T: Send + Sync>() {}
