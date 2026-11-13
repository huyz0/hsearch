//! Structured API errors: every refusal names its type, never a payload.
use serde_json::{Value, json};

/// An HTTP status and an OpenSearch-shaped error body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiError {
    /// HTTP status code.
    pub status: u16,
    /// Stable error type.
    pub kind: &'static str,
    /// Human-readable reason, free of request payloads.
    pub reason: String,
}

impl ApiError {
    pub(crate) fn new(status: u16, kind: &'static str, reason: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            reason: reason.into(),
        }
    }

    pub(crate) fn bad(reason: impl Into<String>) -> Self {
        Self::new(400, "illegal_argument_exception", reason)
    }

    pub(crate) fn parse(reason: impl Into<String>) -> Self {
        Self::new(400, "parsing_exception", reason)
    }

    /// The `error` object, as embedded in a bulk item.
    pub(crate) fn object(&self) -> Value {
        json!({"type": self.kind, "reason": self.reason})
    }

    /// The whole response body.
    pub(crate) fn body(&self) -> Value {
        json!({"error": self.object(), "status": self.status})
    }
}

impl From<hsearch_index::Error> for ApiError {
    fn from(error: hsearch_index::Error) -> Self {
        use hsearch_index::Error as E;
        match error {
            E::NotFound => Self::new(404, "index_not_found_exception", "no such index"),
            E::Exists => Self::new(
                400,
                "resource_already_exists_exception",
                "index already exists",
            ),
            E::Invalid => Self::bad("request refused by the write path"),
            E::Capacity => Self::new(
                429,
                "capacity_exceeded_exception",
                "the index has no room for this request",
            ),
            E::Busy => Self::new(503, "unavailable_exception", "no searchable view right now"),
            E::Unknown => Self::new(
                503,
                "unknown_outcome_exception",
                "the write may or may not be durable; it was not acknowledged",
            ),
            E::AppliedFailed => Self::new(
                500,
                "applied_failed_exception",
                "the write is durable but could not be applied; reopen to recover",
            ),
            E::Fenced | E::Poisoned | E::Closed => Self::new(
                503,
                "writer_unavailable_exception",
                "the index writer stopped and must be reopened",
            ),
            E::Corrupt => Self::new(500, "corrupt_exception", "stored data is not a v0 object"),
            E::Store(_) => Self::new(
                503,
                "store_unavailable_exception",
                "the object store refused",
            ),
            E::Engine(e) => Self::from(e),
        }
    }
}

impl From<hsearch_engine::Error> for ApiError {
    fn from(error: hsearch_engine::Error) -> Self {
        use hsearch_engine::Error as E;
        match error {
            E::Limit => Self::new(
                400,
                "limit_exceeded_exception",
                "a configured limit was exceeded",
            ),
            E::Unsupported => Self::new(400, "unsupported_exception", "unsupported by this node"),
            E::Document | E::Schema | E::Conflict => Self::bad("invalid document or field"),
            E::Busy => Self::new(503, "unavailable_exception", "busy, retry"),
            _ => Self::new(500, "engine_exception", "internal engine failure"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hsearch_index::Error as I;
    use hsearch_object::Error as O;

    #[test]
    fn every_write_path_failure_maps_to_a_named_status() {
        let cases = [
            (I::NotFound, 404),
            (I::Exists, 400),
            (I::Invalid, 400),
            (I::Capacity, 429),
            (I::Busy, 503),
            (I::Unknown, 503),
            (I::AppliedFailed, 500),
            (I::Fenced, 503),
            (I::Poisoned, 503),
            (I::Closed, 503),
            (I::Corrupt, 500),
            (I::Store(O::Unavailable), 503),
            (I::Engine(hsearch_engine::Error::Limit), 400),
        ];
        for (error, status) in cases {
            let api = ApiError::from(error);
            assert_eq!(api.status, status, "{error:?}");
            assert!(!api.kind.is_empty());
            assert_eq!(api.body()["status"], status);
        }
    }

    #[test]
    fn engine_failures_map_without_leaking_detail() {
        use hsearch_engine::Error as E;
        for (error, status) in [
            (E::Limit, 400),
            (E::Unsupported, 400),
            (E::Document, 400),
            (E::Schema, 400),
            (E::Conflict, 400),
            (E::Busy, 503),
            (E::Native, 500),
            (E::Poisoned, 500),
        ] {
            assert_eq!(ApiError::from(error).status, status, "{error:?}");
        }
    }
}
