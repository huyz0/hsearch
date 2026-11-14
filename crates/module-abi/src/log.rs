//! Where module log lines go, the same for both module forms.

use std::sync::Arc;

/// A module log line after the host bounded it, handed to the node's sink.
pub struct LogRecord<'a> {
    /// The module's declared name.
    pub module: &'a str,
    /// The call it was logged from.
    pub call_id: u64,
    /// `HS_LOG_*` as the module gave it.
    pub level: u32,
    /// At most 1 KiB, invalid UTF-8 replaced.
    pub message: &'a str,
}

/// Where module log lines go. Called on the worker thread that is running the
/// module, so it must not block.
pub type LogSink = Arc<dyn Fn(&LogRecord<'_>) + Send + Sync>;
