//! Request limits and the node-wide memory gate for streamed bulk bodies.
//!
//! Memory per request is bounded by a configured buffer, not by the size of the
//! body or of any document (PR-16, PR-40). The gate bounds the sum over
//! concurrent requests: a request that cannot reserve its share is refused with
//! 429 instead of queueing unread bytes in memory.
use std::sync::{Arc, Mutex};

use crate::ApiError;

/// Hard ceiling of one document, bytes (PR-40). Not raisable by configuration.
pub const MAX_DOCUMENT_CEILING: u64 = 200 * 1024 * 1024;
/// Smallest streaming buffer: it must hold one action line and its metadata.
pub const MIN_BUFFER_BYTES: usize = 64 * 1024;
/// Largest streaming buffer: one request may not claim more than this.
pub const MAX_BUFFER_BYTES: usize = 1 << 30;
/// What a request holds beyond its raw buffer: the parsed value and the write
/// operation derived from it, each about one buffer's worth in the worst case.
pub const COST_FACTOR: u64 = 3;

/// Per-profile request limits. Every default is the M02.05 frozen value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Largest document, bytes. At most [`MAX_DOCUMENT_CEILING`].
    pub max_document_bytes: u64,
    /// Largest bulk body, bytes, counting every byte read.
    pub max_bulk_bytes: u64,
    /// Most actions in one bulk.
    pub max_bulk_items: usize,
    /// Raw bytes one bulk request may hold at once: the line being read plus the
    /// batch waiting for its write. A document larger than this is refused
    /// while it is streamed past, never buffered.
    pub buffer_bytes: usize,
    /// Reserved bytes across all concurrent bulk requests.
    pub memory_budget_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_document_bytes: 16 * 1024 * 1024,
            max_bulk_bytes: 64 * 1024 * 1024,
            max_bulk_items: 5000,
            buffer_bytes: 8 * 1024 * 1024,
            memory_budget_bytes: 256 * 1024 * 1024,
        }
    }
}

impl Limits {
    /// Refuse a configuration that cannot work, rather than clamping it.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_document_bytes == 0 || self.max_document_bytes > MAX_DOCUMENT_CEILING {
            return Err(format!(
                "the document limit must be 1..={MAX_DOCUMENT_CEILING} bytes; 200 MiB is a hard ceiling"
            ));
        }
        if self.max_bulk_bytes < self.max_document_bytes {
            return Err("the bulk body limit must hold at least one maximal document".into());
        }
        if self.max_bulk_items == 0 {
            return Err("the bulk item limit must be positive".into());
        }
        if self.buffer_bytes > MAX_BUFFER_BYTES {
            return Err(format!(
                "the buffer must be at most {MAX_BUFFER_BYTES} bytes"
            ));
        }
        if self.buffer_bytes < MIN_BUFFER_BYTES {
            return Err(format!(
                "the buffer must be at least {MIN_BUFFER_BYTES} bytes"
            ));
        }
        if self.memory_budget_bytes < self.request_cost(u64::MAX) {
            return Err("the memory budget must admit at least one full request".into());
        }
        Ok(())
    }

    /// Bytes a request reserves: its share for a body of `body_bytes`, at most a
    /// full buffer's worth. A small body costs little, so small bulks stay concurrent.
    #[must_use]
    pub fn request_cost(&self, body_bytes: u64) -> u64 {
        COST_FACTOR.saturating_mul(body_bytes.min(self.buffer_bytes as u64))
            + MIN_BUFFER_BYTES as u64
    }
}

/// Shared pool of reservable bytes.
#[derive(Debug)]
pub(crate) struct MemoryGate {
    free: Arc<Mutex<u64>>,
}

/// A reservation, returned to the pool when dropped.
#[derive(Debug)]
pub(crate) struct Permit {
    free: Arc<Mutex<u64>>,
    bytes: u64,
}

impl MemoryGate {
    pub(crate) fn new(bytes: u64) -> Self {
        Self {
            free: Arc::new(Mutex::new(bytes)),
        }
    }

    /// Reserve `bytes` or refuse at once; never wait holding a request's socket.
    pub(crate) fn reserve(&self, bytes: u64) -> Result<Permit, ApiError> {
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        if *free < bytes {
            return Err(ApiError::new(
                429,
                "es_rejected_execution_exception",
                "the node's bulk memory budget is in use; retry after other bulk requests finish",
            ));
        }
        *free -= bytes;
        Ok(Permit {
            free: Arc::clone(&self.free),
            bytes,
        })
    }

    #[cfg(test)]
    pub(crate) fn available(&self) -> u64 {
        *self.free.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        *free += self.bytes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_the_ceiling_is_not_raisable() {
        let limits = Limits::default();
        assert_eq!(limits.validate(), Ok(()));
        assert_eq!(limits.max_document_bytes, 16 * 1024 * 1024);
        let ceiling = Limits {
            max_document_bytes: MAX_DOCUMENT_CEILING,
            max_bulk_bytes: MAX_DOCUMENT_CEILING,
            ..limits
        };
        assert_eq!(ceiling.validate(), Ok(()));
        let over = Limits {
            max_document_bytes: MAX_DOCUMENT_CEILING + 1,
            max_bulk_bytes: u64::MAX,
            ..limits
        };
        assert!(over.validate().is_err());
    }

    #[test]
    fn impossible_configurations_are_refused() {
        let base = Limits::default();
        for bad in [
            Limits {
                max_document_bytes: 0,
                ..base
            },
            Limits {
                max_bulk_bytes: base.max_document_bytes - 1,
                ..base
            },
            Limits {
                max_bulk_items: 0,
                ..base
            },
            Limits {
                buffer_bytes: MIN_BUFFER_BYTES - 1,
                ..base
            },
            Limits {
                memory_budget_bytes: 1,
                ..base
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cost_grows_with_the_body_up_to_one_buffer() {
        let limits = Limits::default();
        assert!(limits.request_cost(10) < limits.request_cost(1 << 20));
        assert_eq!(
            limits.request_cost(limits.buffer_bytes as u64),
            limits.request_cost(u64::MAX)
        );
    }

    #[test]
    fn the_gate_refuses_when_empty_and_refills_on_drop() {
        let gate = MemoryGate::new(100);
        let a = gate.reserve(60).unwrap();
        assert_eq!(gate.reserve(60).unwrap_err().status, 429);
        let b = gate.reserve(40).unwrap();
        assert_eq!(gate.available(), 0);
        drop(a);
        assert_eq!(gate.available(), 60);
        drop(b);
        assert_eq!(gate.available(), 100);
    }
}
