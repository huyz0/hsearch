//! One call into an extension point, the same for both module forms.

use crate::Kind;
use std::time::Duration;

/// One call into an extension point. Both hosts take this and return bytes or a
/// [`crate::CallError`].
pub struct Request<'a> {
    /// The point's kind.
    pub kind: Kind,
    /// The point's id, as declared.
    pub point: &'a str,
    /// The point's input encoding, borrowed for the call.
    pub input: &'a [u8],
    /// The whole call's deadline, from submission: queue wait, run time and the
    /// cooperative cancel. Also the `timeout_ms` the module is told.
    pub timeout: Duration,
    /// The first output buffer: the largest result the point's bounds allow.
    pub output_bound: u32,
    /// The most the host will retry a too-small buffer up to; at least `output_bound`.
    pub output_limit: u32,
}
