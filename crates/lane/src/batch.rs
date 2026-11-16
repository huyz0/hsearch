//! Dirty-only batching: records from many indexes of one lane are grouped into slots by count,
//! bytes and a maximum wait. No clock is read here (the caller passes `now`), and an empty batcher
//! has **no deadline**, so an idle lane costs no timer, no poll and no request.
use std::time::Duration;

use hsearch_format::{
    Digest, Id, MAX_INLINE_RECORD_BYTES, MAX_SLOT_RECORDS, PayloadRef, RecordOp, RecordSpec,
};

use crate::{Error, Result};

/// Fixed part of an encoded record: index 16, seq 8, op 1, payload kind 1.
const RECORD_FIXED: usize = 26;

/// A record's payload, owned until the slot is encoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OwnedPayload {
    /// A delete.
    None,
    /// Document bytes carried in the slot, at most [`MAX_INLINE_RECORD_BYTES`].
    Inline(Vec<u8>),
    /// A document written to its own object first (the caller owns that write).
    External {
        /// The payload object.
        object: Id,
        /// Its length.
        length: u64,
        /// Its SHA-256.
        digest: Digest,
    },
}

/// One record waiting for a slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedRecord {
    /// The index the record belongs to.
    pub index: Id,
    /// The index's own sequence for the record.
    pub seq: u64,
    /// Put or delete.
    pub op: RecordOp,
    /// The document or its location.
    pub payload: OwnedPayload,
}

impl OwnedRecord {
    /// The borrowed form the slot encoder takes.
    #[must_use]
    pub fn spec(&self) -> RecordSpec<'_> {
        RecordSpec {
            index: self.index,
            seq: self.seq,
            op: self.op,
            payload: match &self.payload {
                OwnedPayload::None => PayloadRef::None,
                OwnedPayload::Inline(bytes) => PayloadRef::Inline(bytes),
                OwnedPayload::External {
                    object,
                    length,
                    digest,
                } => PayloadRef::External {
                    object: *object,
                    length: *length,
                    digest: *digest,
                },
            },
        }
    }

    /// Encoded size in the slot: what the byte target is compared against.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        RECORD_FIXED
            + match &self.payload {
                OwnedPayload::None => 0,
                OwnedPayload::Inline(bytes) => 4 + bytes.len(),
                OwnedPayload::External { .. } => 16 + 8 + 32,
            }
    }
}

/// How a lane groups writes. Each limit is a cost dial: a longer wait fills slots and cuts PUTs, at
/// the price of acknowledgement latency (M02.07: 250 ms for the resilient profile, 1 s for economy).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchPolicy {
    /// Most records in a slot, at most the format's 4096.
    pub max_records: usize,
    /// Byte target for a slot (4 MiB, M02.05 PR-40). A single record above it is its own batch.
    pub max_bytes: usize,
    /// Longest the oldest record waits before its batch closes.
    pub max_wait: Duration,
}

impl Default for BatchPolicy {
    fn default() -> Self {
        Self {
            max_records: MAX_SLOT_RECORDS,
            max_bytes: 4 << 20,
            max_wait: Duration::from_millis(250),
        }
    }
}

/// A closed group of records, ready for one append.
#[derive(Debug, Eq, PartialEq)]
pub struct Batch {
    /// The records, in arrival order.
    pub records: Vec<OwnedRecord>,
    /// Their encoded size.
    pub bytes: usize,
    /// When the oldest record arrived.
    pub opened: Duration,
}

impl Batch {
    /// The borrowed records for `LaneWriter::append`.
    #[must_use]
    pub fn specs(&self) -> Vec<RecordSpec<'_>> {
        self.records.iter().map(OwnedRecord::spec).collect()
    }
}

/// Groups records into batches. Memory is bounded by the policy: a batch closes before it passes
/// `max_bytes` or `max_records`, so at most one batch's worth plus one record is ever held.
#[derive(Debug)]
pub struct Batcher {
    policy: BatchPolicy,
    pending: Vec<OwnedRecord>,
    bytes: usize,
    opened: Option<Duration>,
}

impl Batcher {
    /// A batcher under `policy`; limits of zero or beyond the format are refused.
    pub fn new(policy: BatchPolicy) -> Result<Self> {
        if policy.max_records == 0
            || policy.max_records > MAX_SLOT_RECORDS
            || policy.max_bytes == 0
            || policy.max_bytes > hsearch_format::MAX_SLOT_BODY_BYTES
        {
            return Err(Error::Invalid("batch policy"));
        }
        Ok(Self {
            policy,
            pending: Vec::new(),
            bytes: 0,
            opened: None,
        })
    }

    fn take(&mut self) -> Option<Batch> {
        let opened = self.opened.take()?;
        let records = std::mem::take(&mut self.pending);
        let bytes = std::mem::take(&mut self.bytes);
        Some(Batch {
            records,
            bytes,
            opened,
        })
    }

    /// Add a record at time `now`. Returns the batches this closed (zero, one or two): the pending
    /// one if this record would overflow it, and the record's own if it alone exceeds the byte
    /// target.
    pub fn push(&mut self, record: OwnedRecord, now: Duration) -> Result<Vec<Batch>> {
        if let OwnedPayload::Inline(bytes) = &record.payload
            && (bytes.is_empty() || bytes.len() > MAX_INLINE_RECORD_BYTES)
        {
            return Err(Error::Invalid("inline record size"));
        }
        let cost = record.encoded_len();
        let mut closed = Vec::new();
        let overflows = self.bytes + cost > self.policy.max_bytes
            || self.pending.len() >= self.policy.max_records;
        if overflows && let Some(batch) = self.take() {
            closed.push(batch);
        }
        self.opened.get_or_insert(now);
        self.bytes += cost;
        self.pending.push(record);
        if self.bytes >= self.policy.max_bytes || self.pending.len() >= self.policy.max_records {
            closed.extend(self.take());
        }
        Ok(closed)
    }

    /// Close the pending batch if its oldest record has waited `max_wait`.
    pub fn poll(&mut self, now: Duration) -> Option<Batch> {
        let opened = self.opened?;
        if now.saturating_sub(opened) >= self.policy.max_wait {
            self.take()
        } else {
            None
        }
    }

    /// When the pending batch must close, or `None` if nothing is pending: an idle lane has no
    /// deadline, so nothing needs to wake for it.
    #[must_use]
    pub fn deadline(&self) -> Option<Duration> {
        self.opened.map(|opened| opened + self.policy.max_wait)
    }

    /// Close whatever is pending now (shutdown, or a durable-now request).
    pub fn flush(&mut self) -> Option<Batch> {
        self.take()
    }

    /// Records waiting.
    #[must_use]
    pub fn pending_records(&self) -> usize {
        self.pending.len()
    }

    /// Encoded bytes waiting.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.bytes
    }
}
