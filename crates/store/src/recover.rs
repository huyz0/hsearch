//! Recovering one index from the object store alone: nothing local is consulted.
//!
//! Recovery reads the lane's structural head, verifies the base manifest it names (if any), then
//! streams the committed log from the head's cover to the first absent slot, handing this index's
//! records to the caller in sequence order. It refuses to call the index ready unless the history
//! is whole: a sequence gap, a different body under a repeated identity, a corrupt slot, a log that
//! ends below a position the caller knows was acknowledged, or a replay larger than its bounds each
//! block readiness with a named reason. Memory is one slot plus a digest map bounded by the caller's record limit. A record below the
//! base is accepted without comparison (the base holds it), and a sealed index's successor lane is
//! not followed: a caller that needs either passes `Expect`.
use std::collections::BTreeMap;

use hsearch_format::{Id, PayloadRef, RecordOp, SlotKind, StructuralHead};
use hsearch_index::barrier::{Replay, Step};
use hsearch_lane::{Error as LaneError, LaneId, block_on, read_head, read_log};
use hsearch_object::{LargeObjectWriter, MetadataStore};
use sha2::{Digest as _, Sha256};

use crate::publish::{Dependencies, Error as PublishError, Publisher};

/// Bounds on one recovery's replay: a larger tail must be checkpointed instead of replayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplayLimits {
    /// Most slots read.
    pub slots: u64,
    /// Most of this index's records applied.
    pub records: u64,
    /// Most body bytes of this index's records applied.
    pub bytes: u64,
}

/// What recovery must reach before it may call the index ready.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Expect {
    /// A lane position the log must reach: the first absent slot must be at or above it. Taken
    /// from the highest acknowledged receipt the caller knows (or an authority barrier).
    pub ordinal: u64,
    /// The index sequence the replay must reach: the applied prefix must be at least this.
    pub seq: u64,
}

/// What to recover.
pub struct Target<'a> {
    /// The lane the index lives in.
    pub lane: &'a LaneId,
    /// The index.
    pub index: Id,
    /// The shard of the index's manifests.
    pub shard: Id,
    /// The shard incarnation.
    pub incarnation: u64,
    /// Replay bounds.
    pub limits: ReplayLimits,
    /// What must be reached.
    pub expect: Expect,
}

/// One committed record handed to the caller, in sequence order.
pub struct Record<'a> {
    /// The index sequence.
    pub seq: u64,
    /// Whether this deletes the document.
    pub delete: bool,
    /// The body (empty for a delete).
    pub body: &'a [u8],
    /// The slot it was found in.
    pub ordinal: u64,
}

/// Why the index is not ready. Every variant blocks readiness; none is guessed around.
#[derive(Debug, Eq, PartialEq)]
pub enum NotReady {
    /// The lane has no structural head.
    NoHead,
    /// The head authorises a different activation than the one asked for.
    Activation,
    /// The base manifest the head names cannot be verified.
    Base(String),
    /// A sequence of this index is missing: `expected` was next, `found` came.
    Gap {
        /// The sequence that was due.
        expected: u64,
        /// The sequence that arrived.
        found: u64,
    },
    /// A sequence appears twice with different contents.
    Conflict(u64),
    /// A slot is not a valid lane object.
    Corrupt,
    /// The log ends below a position known to be committed, or the applied prefix is below the
    /// expected sequence.
    Missing,
    /// The replay is over a bound: take a checkpoint.
    Limit,
    /// The caller's apply failed.
    Apply,
    /// A record this recovery cannot replay (an external payload): the log is not recoverable
    /// here, and skipping it would lose a document.
    Unsupported(u64),
    /// The store failed; retry.
    Store(String),
}

/// A recovered index.
#[derive(Debug, Eq, PartialEq)]
pub struct Recovered {
    /// The base manifest replayed on top of, if the head names one.
    pub base: Option<hsearch_format::ManifestRoot>,
    /// The first sequence of this index not in the base.
    pub from_seq: u64,
    /// The applied prefix: sequences below this are in the base or were applied.
    pub applied: u64,
    /// Records of this index applied from the log.
    pub replayed: u64,
    /// Slots read.
    pub slots: u64,
    /// The first absent lane position: where the log ends.
    pub tail: u64,
    /// The head generation recovery started from.
    pub head_generation: u64,
}

fn body_digest(delete: bool, body: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([u8::from(delete)]);
    h.update(body);
    h.finalize().into()
}

/// Recover `target`, calling `apply` on each of its records after the base, in sequence order.
pub fn recover<S, D, F>(
    publisher: &Publisher<S>,
    target: &Target<'_>,
    dependencies: &D,
    mut apply: F,
) -> Result<Recovered, NotReady>
where
    S: MetadataStore + LargeObjectWriter + 'static,
    D: Dependencies,
    F: FnMut(&Record<'_>) -> Result<(), ()>,
{
    let store = publisher.store();
    let layout = publisher.layout();
    let (head, _): (StructuralHead, _) = match block_on(read_head(store, layout, target.lane)) {
        Ok(found) => found,
        Err(LaneError::NoHead) => return Err(NotReady::NoHead),
        Err(LaneError::Format(_)) => return Err(NotReady::Corrupt),
        Err(e) => return Err(NotReady::Store(format!("{e:?}"))),
    };
    if head.activation != target.lane.activation {
        return Err(NotReady::Activation);
    }
    let base = match head.checkpoint {
        None => None,
        Some(manifest) => Some(
            publisher
                .verify_root(
                    &target.index,
                    &target.shard,
                    target.incarnation,
                    &manifest,
                    dependencies,
                )
                .map_err(|e| match e {
                    // An object the base names is absent: committed history is missing, which is
                    // not a transient failure to retry.
                    PublishError::Store(hsearch_object::Error::NotFound) => {
                        NotReady::Base(format!("{e:?}"))
                    }
                    PublishError::Store(_) => NotReady::Store(format!("{e:?}")),
                    other => NotReady::Base(format!("{other:?}")),
                })?,
        ),
    };
    let from_seq = base.as_ref().map_or(0, |b| b.covered);
    let mut replay = Replay::new(from_seq);
    let mut seen: BTreeMap<u64, [u8; 32]> = BTreeMap::new();
    let (mut slots, mut replayed, mut bytes) = (0u64, 0u64, 0u64);
    let mut failure: Option<NotReady> = None;
    let end = block_on(read_log(
        store,
        layout,
        target.lane,
        head.cover,
        None,
        |view| {
            slots += 1;
            if slots > target.limits.slots {
                failure = Some(NotReady::Limit);
                return Err(LaneError::Invalid("replay bound"));
            }
            if view.kind != SlotKind::Data {
                return Ok(());
            }
            for record in view.records() {
                if record.index != target.index {
                    continue;
                }
                let (delete, body): (bool, &[u8]) = match (record.op, record.payload) {
                    (RecordOp::Put, PayloadRef::Inline(b)) => (false, b),
                    (RecordOp::Delete, PayloadRef::None) => (true, &[]),
                    // A seal repeats the last sequence and carries no document.
                    (RecordOp::Seal, _) => continue,
                    (RecordOp::Put, PayloadRef::External { .. }) => {
                        failure = Some(NotReady::Unsupported(record.seq));
                        return Err(LaneError::Invalid("external"));
                    }
                    _ => {
                        failure = Some(NotReady::Corrupt);
                        return Err(LaneError::Invalid("malformed"));
                    }
                };
                let digest = body_digest(delete, body);
                match replay.begin(record.seq) {
                    Ok(Step::Apply) => {
                        replayed += 1;
                        bytes += body.len() as u64;
                        if replayed > target.limits.records || bytes > target.limits.bytes {
                            failure = Some(NotReady::Limit);
                            return Err(LaneError::Invalid("replay bound"));
                        }
                        let record = Record {
                            seq: record.seq,
                            delete,
                            body,
                            ordinal: view.position.ordinal,
                        };
                        if apply(&record).is_err() || replay.done(record.seq).is_err() {
                            replay.fail();
                            failure = Some(NotReady::Apply);
                            return Err(LaneError::Invalid("apply"));
                        }
                        seen.insert(record.seq, digest);
                    }
                    Ok(Step::Duplicate) => {
                        // Seen in this replay with other contents is corruption; below the base
                        // there is nothing to compare against and the base already holds it.
                        if seen.get(&record.seq).is_some_and(|d| *d != digest) {
                            failure = Some(NotReady::Conflict(record.seq));
                            return Err(LaneError::Invalid("conflict"));
                        }
                    }
                    Ok(Step::Gap { expected }) => {
                        failure = Some(NotReady::Gap {
                            expected,
                            found: record.seq,
                        });
                        return Err(LaneError::Invalid("gap"));
                    }
                    Err(_) => {
                        failure = Some(NotReady::Apply);
                        return Err(LaneError::Invalid("replay"));
                    }
                }
            }
            Ok(())
        },
    ));
    let tail = match end {
        Ok(end) => end,
        Err(e) => {
            return Err(failure.unwrap_or(match e {
                LaneError::Format(_) => NotReady::Corrupt,
                other => NotReady::Store(format!("{other:?}")),
            }));
        }
    };
    // A hole is the end of the log only if no checkpoint has since reclaimed it: re-read the head
    // and refuse if it moved past where the scan stopped.
    let (after, _): (StructuralHead, _) = match block_on(read_head(store, layout, target.lane)) {
        Ok(found) => found,
        Err(LaneError::Format(_)) => return Err(NotReady::Corrupt),
        Err(e) => return Err(NotReady::Store(format!("{e:?}"))),
    };
    if after.cover > tail || after.activation != head.activation {
        return Err(NotReady::Missing);
    }
    let applied = replay.applied();
    if tail < target.expect.ordinal || tail < head.tail.ordinal || applied < target.expect.seq {
        return Err(NotReady::Missing);
    }
    Ok(Recovered {
        base,
        from_seq,
        applied,
        replayed,
        slots,
        tail,
        head_generation: head.generation,
    })
}
