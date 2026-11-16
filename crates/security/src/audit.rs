//! The audit schema for key use. One record per provider call, activation, invalidation and refusal.
//!
//! A record names the index, key reference, key version and outcome. It has no field that can hold key
//! material, and its JSON form is built from validated identifiers only, so nothing needs escaping. Cache
//! hits are counted, not recorded: a record per hit would put an allocation and a lock on the read path.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ids::IndexId;
use crate::keys::{KeyRef, KeyVersion};

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditOp {
    /// A data key was wrapped.
    Wrap,
    /// A data key was unwrapped by the provider (a cache miss).
    Unwrap,
    /// A key was checked before being assigned to an index.
    Activate,
    /// Cached data keys were dropped.
    Invalidate,
    /// A request was refused before reaching the provider.
    Refused,
    /// A customer key gained a version.
    Rotate,
    /// A revocation was applied: the index's cached keys were dropped.
    Revoke,
    /// A customer key was destroyed (crypto-shredding).
    Destroy,
}

impl AuditOp {
    fn as_str(self) -> &'static str {
        match self {
            AuditOp::Wrap => "wrap",
            AuditOp::Unwrap => "unwrap",
            AuditOp::Activate => "activate",
            AuditOp::Invalidate => "invalidate",
            AuditOp::Refused => "refused",
            AuditOp::Rotate => "rotate",
            AuditOp::Revoke => "revoke",
            AuditOp::Destroy => "destroy",
        }
    }
}

/// One audit record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    /// Wall-clock time.
    pub at: SystemTime,
    /// What happened.
    pub op: AuditOp,
    /// The index (tenant).
    pub index: IndexId,
    /// The customer key, when one was involved.
    pub key: Option<KeyRef>,
    /// The key version, when known.
    pub version: Option<KeyVersion>,
    /// The policy generation the operation ran under.
    pub generation: u64,
    /// `ok`, or the error's stable code.
    pub outcome: &'static str,
}

impl AuditEvent {
    /// One JSON object, no newline. Every field is a validated identifier or a fixed word.
    pub fn to_json(&self) -> String {
        let ms = self
            .at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut out = format!(
            "{{\"at_ms\":{ms},\"op\":\"{}\",\"index\":\"{}\"",
            self.op.as_str(),
            self.index
        );
        if let Some(k) = &self.key {
            out.push_str(&format!(",\"key\":\"{k}\""));
        }
        if let Some(v) = &self.version {
            out.push_str(&format!(",\"version\":\"{}\"", v.as_str()));
        }
        out.push_str(&format!(
            ",\"generation\":{},\"outcome\":\"{}\"}}",
            self.generation, self.outcome
        ));
        out
    }
}

/// Where records go. `record` must not block on I/O: a slow sink would stall the key path.
pub trait AuditSink: Send + Sync {
    /// Takes one record.
    fn record(&self, event: AuditEvent);
}

/// A bounded in-memory sink that drops its oldest record when full and counts what it dropped.
pub struct MemoryAuditSink {
    cap: usize,
    events: Mutex<VecDeque<AuditEvent>>,
    dropped: AtomicU64,
}

impl MemoryAuditSink {
    /// A sink holding at most `cap` records.
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            events: Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// The records held, oldest first.
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// How many records were dropped to stay within the bound.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl AuditSink for MemoryAuditSink {
    fn record(&self, event: AuditEvent) {
        let mut events = self.events.lock().unwrap_or_else(|p| p.into_inner());
        if events.len() >= self.cap {
            events.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        events.push_back(event);
    }
}
