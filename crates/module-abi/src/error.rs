//! Typed failures of section 5. A failing module never falls back silently.

use crate::{Kind, status};
use std::fmt;

/// Most module-supplied detail bytes kept (`HS_MAX_ERROR_DETAIL_BYTES`).
pub const MAX_ERROR_DETAIL_BYTES: usize = 256;

/// The outcome classes of section 5, with the retry guidance fixed per class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    /// No slot to run in before the wait deadline. Retry with backoff.
    Busy,
    /// The module returned a negative status.
    Error,
    /// The output failed host validation.
    OutputMalformed,
    /// A host-enforced limit (size, time, memory, fuel) was exceeded.
    Limit,
    /// The instance trapped (WASM only).
    Trapped,
    /// The module declared a version the host does not support.
    VersionRefused,
    /// The module is failed, disabled or not loaded.
    Unavailable,
}

impl FailureKind {
    /// Only `Busy` is retriable by the client.
    #[must_use]
    pub const fn retriable(self) -> bool {
        matches!(self, Self::Busy)
    }

    /// The error name used in logs and responses.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Busy => "ModuleBusy",
            Self::Error => "ModuleError",
            Self::OutputMalformed => "ModuleOutputMalformed",
            Self::Limit => "ModuleLimit",
            Self::Trapped => "ModuleTrapped",
            Self::VersionRefused => "ModuleVersionRefused",
            Self::Unavailable => "ModuleUnavailable",
        }
    }
}

/// A failed call: the module, the point, the class, the status and a fixed
/// reason. Module-supplied `detail` is for logs and never part of `Display`, so
/// it cannot reach a client unredacted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallError {
    /// The module's declared name.
    pub module: String,
    /// The point's kind, when the call got that far.
    pub kind: Option<Kind>,
    /// The point id, when the call got that far.
    pub point: Option<String>,
    /// The outcome class.
    pub failure: FailureKind,
    /// The module's status code, when it returned one.
    pub status: Option<i32>,
    /// A fixed host-side reason, never module text.
    pub reason: &'static str,
    /// Module text, at most [`MAX_ERROR_DETAIL_BYTES`] of valid UTF-8, logs only.
    pub detail: Option<String>,
}

impl CallError {
    /// Builds an error with no point, status or detail.
    #[must_use]
    pub fn new(module: &str, failure: FailureKind, reason: &'static str) -> Self {
        Self {
            module: module.to_owned(),
            kind: None,
            point: None,
            failure,
            status: None,
            reason,
            detail: None,
        }
    }

    /// Names the point the call was for.
    #[must_use]
    pub fn at(mut self, kind: Kind, point: &str) -> Self {
        self.kind = Some(kind);
        self.point = Some(point.to_owned());
        self
    }

    /// Records the status the module returned.
    #[must_use]
    pub const fn with_status(mut self, code: i32) -> Self {
        self.status = Some(code);
        self
    }

    /// Keeps module text, cut to the bound at a character boundary.
    #[must_use]
    pub fn with_detail(mut self, bytes: &[u8]) -> Self {
        let cut = &bytes[..bytes.len().min(MAX_ERROR_DETAIL_BYTES)];
        let valid = match std::str::from_utf8(cut) {
            Ok(text) => text,
            Err(e) => std::str::from_utf8(&cut[..e.valid_up_to()]).unwrap_or(""),
        };
        self.detail = (!valid.is_empty()).then(|| valid.to_owned());
        self
    }

    /// Whether the client may retry.
    #[must_use]
    pub const fn retriable(&self) -> bool {
        self.failure.retriable()
    }

    /// The failure for a negative module status.
    #[must_use]
    pub fn from_status(module: &str, code: i32) -> Self {
        let reason = match code {
            status::INVALID_ARGUMENT => "module rejected the input",
            status::UNSUPPORTED => "module does not support the point",
            status::LIMIT => "module refused an over-limit input",
            status::CANCELLED => "module stopped on cancellation",
            status::MODULE => "module reported a failure",
            status::NOT_INITIALIZED => "module was not initialised",
            _ => "module returned a failure status",
        };
        Self::new(module, FailureKind::Error, reason).with_status(code)
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} module={}", self.failure.name(), self.module)?;
        if let (Some(kind), Some(point)) = (self.kind, &self.point) {
            write!(f, " point={}:{}", kind.name(), point)?;
        }
        if let Some(code) = self.status {
            write!(f, " status={code}")?;
        }
        write!(f, ": {}", self.reason)
    }
}

impl std::error::Error for CallError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_busy_is_retriable() {
        for kind in [
            FailureKind::Error,
            FailureKind::OutputMalformed,
            FailureKind::Limit,
            FailureKind::Trapped,
            FailureKind::VersionRefused,
            FailureKind::Unavailable,
        ] {
            assert!(!kind.retriable(), "{}", kind.name());
        }
        assert!(FailureKind::Busy.retriable());
        assert!(CallError::new("m", FailureKind::Busy, "r").retriable());
    }

    #[test]
    fn display_never_carries_module_text() {
        let e = CallError::from_status("m", status::MODULE)
            .at(Kind::Query, "q")
            .with_detail(b"secret path /etc/passwd");
        let shown = e.to_string();
        assert_eq!(
            shown,
            "ModuleError module=m point=query:q status=-6: module reported a failure"
        );
        assert!(!shown.contains("secret"));
        assert_eq!(e.detail.as_deref(), Some("secret path /etc/passwd"));
    }

    #[test]
    fn detail_is_cut_to_the_bound_and_to_valid_utf8() {
        let long = "é".repeat(200);
        let e = CallError::new("m", FailureKind::Error, "r").with_detail(long.as_bytes());
        let kept = e.detail.expect("kept");
        assert!(kept.len() <= MAX_ERROR_DETAIL_BYTES);
        assert_eq!(kept.len(), 256);
        assert!(kept.chars().all(|c| c == 'é'));
        let odd = CallError::new("m", FailureKind::Error, "r").with_detail(&[b'a', 0xff, b'b']);
        assert_eq!(odd.detail.as_deref(), Some("a"));
        assert_eq!(
            CallError::new("m", FailureKind::Error, "r")
                .with_detail(&[0xff])
                .detail,
            None
        );
        assert_eq!(
            CallError::new("m", FailureKind::Error, "r")
                .with_detail(b"")
                .detail,
            None
        );
    }

    #[test]
    fn statuses_map_to_fixed_reasons() {
        for code in [
            status::INVALID_ARGUMENT,
            status::UNSUPPORTED,
            status::LIMIT,
            status::CANCELLED,
            status::MODULE,
            status::NOT_INITIALIZED,
            -99,
        ] {
            let e = CallError::from_status("m", code);
            assert_eq!((e.failure, e.status), (FailureKind::Error, Some(code)));
            assert!(!e.reason.is_empty());
        }
    }

    #[test]
    fn defined_statuses() {
        assert!(status::is_defined(0));
        assert!(status::is_defined(-7));
        assert!(!status::is_defined(1));
        assert!(!status::is_defined(-8));
    }
}
