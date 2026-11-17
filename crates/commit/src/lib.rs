//! The group committer over a fenced lane (M06.01).
//!
//! A [`Committer`] owns one [`hsearch_lane::LaneWriter`] and turns a call's resolved operations into
//! acknowledged slots:
//!
//! * **Checked before the commit PUT.** Each request names the schema, crypto and resolved-operation
//!   digests it was prepared against; a [`DependencyView`] says whether they are still current. A stale
//!   request is [`Reject::Stale`] and nothing is written for it.
//! * **Dirty only.** A slot holds the records of the call and nothing else: an index with no operation
//!   adds no byte and no request, and a committer with nothing to commit makes no request at all.
//! * **One append in flight, in order.** Slots are appended one after another. After any slot that did
//!   not commit, no later slot of the call is attempted ([`Outcome::NotAttempted`]), so a failed or
//!   ambiguous append is never followed by another.
//! * **Explicit outcomes.** [`Outcome::Committed`] carries a [`Receipt`] naming the slot and position;
//!   [`Outcome::Unknown`] says the slot may exist; nothing is acknowledged on a guess. An unknown
//!   outcome is settled by [`Committer::recover`], which reopens the lane (fencing any straggler) and
//!   reads the log: the record is either there ([`Resolution::Committed`]) or, now that the fence is
//!   in place, can never appear ([`Resolution::NotCommitted`]).
//!
//! Encryption of record payloads is the caller's: a `Request`'s body is what goes in the slot. The
//! per-commit unique token the fence needs is the lane's (every slot carries a fresh one).
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use hsearch_format::{
    Id, Layout, MAX_INLINE_RECORD_BYTES, PayloadRef, RecordOp, SlotKind, SlotView,
};
use hsearch_lane::batch::{BatchPolicy, Batcher, OwnedPayload, OwnedRecord};
use hsearch_lane::{Error as LaneError, LaneId, LaneWriter, OpenReport};
use hsearch_object::MetadataStore;
use sha2::{Digest, Sha256};

/// Where a replayed record was found: slot ordinal, position in the slot, request digest.
type Found = (u64, u32, Fingerprint);

/// A digest of something a request depends on.
pub type Fingerprint = [u8; 32];

/// What a request was prepared against. Each must still be current when the commit happens.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Dependencies {
    /// The mapping/schema the operation was resolved under.
    pub schema: Fingerprint,
    /// The key (reference and version) its payload is protected under.
    pub crypto: Fingerprint,
    /// The resolved operation set it depends on (for example the version it was conditioned on).
    pub resolved: Fingerprint,
}

/// Which dependency was stale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stale {
    /// The mapping changed.
    Schema,
    /// The key or its version changed.
    Crypto,
    /// What the operation was resolved against changed.
    Resolved,
}

/// The committer's view of what is current. Called once per request, before anything is written.
pub trait DependencyView {
    /// `Ok` when every dependency of `index` in `deps` is current.
    fn check(&self, index: &Id, deps: &Dependencies) -> Result<(), Stale>;
}

/// A view that accepts everything (a lane with no dependency to check).
#[derive(Clone, Copy, Debug, Default)]
pub struct AnyDependencies;

impl DependencyView for AnyDependencies {
    fn check(&self, _: &Id, _: &Dependencies) -> Result<(), Stale> {
        Ok(())
    }
}

/// One resolved operation to commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    /// The index it belongs to.
    pub index: Id,
    /// The index's own sequence for this operation; unique per index within a lane.
    pub seq: u64,
    /// A put (non-empty `body`) or a delete (empty `body`).
    pub body: Vec<u8>,
    /// Whether this deletes the document.
    pub delete: bool,
    /// What it was prepared against.
    pub deps: Dependencies,
}

impl Request {
    /// The digest receipts and resolution match on: index, sequence, kind and body.
    #[must_use]
    pub fn digest(&self) -> Fingerprint {
        digest_of(&self.index, self.seq, self.delete, &self.body)
    }
}

fn digest_of(index: &Id, seq: u64, delete: bool, body: &[u8]) -> Fingerprint {
    let mut h = Sha256::new();
    h.update(b"hs-commit-request\x01");
    h.update(index);
    h.update(seq.to_le_bytes());
    h.update([u8::from(delete)]);
    h.update((body.len() as u64).to_le_bytes());
    h.update(body);
    h.finalize().into()
}

/// Which commit protocol acknowledged a record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    /// One conditional create of the next slot of a fenced lane.
    FencedSlot,
}

/// Proof that a record is in the lane: where, and which request it was.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Receipt {
    /// The index.
    pub index: Id,
    /// The index's sequence.
    pub seq: u64,
    /// [`Request::digest`] of the committed request.
    pub request_digest: Fingerprint,
    /// The lane's activation.
    pub activation: u64,
    /// The slot's position.
    pub ordinal: u64,
    /// The record's position inside the slot.
    pub batch_position: u32,
    /// The protocol that committed it.
    pub protocol: Protocol,
}

/// Why a request was refused with nothing written for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reject {
    /// A dependency was stale.
    Stale(Stale),
    /// The body is empty for a put, present for a delete, or over the inline bound.
    BadBody,
    /// The same (index, sequence) appeared twice in the call.
    Duplicate,
    /// The index is not a member of the lane, or is sealed in it.
    NotAdmitted,
    /// The store certainly did not accept the slot.
    Store,
    /// The slot position was taken. Usually another writer's, but a provider that cannot resolve a
    /// retried create by identity reports our own accepted slot the same way, so this is **not** proof
    /// that nothing was written: settle it with [`Committer::recover`] using [`Pending::of`].
    Fenced,
    /// The committer is poisoned by an earlier slot; reopen.
    Poisoned,
}

/// What happened to one request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Durable, in order, with its receipt.
    Committed(Receipt),
    /// Refused. Nothing was written for it, except [`Reject::Fenced`], which recover must settle.
    Rejected(Reject),
    /// An earlier slot of this call did not commit, so this one was never sent. Safe to retry.
    NotAttempted,
    /// The slot at `ordinal` may or may not exist. Not acknowledged. Settle with [`Committer::recover`].
    Unknown {
        /// The slot position that was being written.
        ordinal: u64,
    },
}

/// A request whose outcome was unknown, kept for resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pending {
    /// The index.
    pub index: Id,
    /// The sequence.
    pub seq: u64,
    /// [`Request::digest`] of the request.
    pub request_digest: Fingerprint,
}

impl Pending {
    /// The pending identity of `request`.
    #[must_use]
    pub fn of(request: &Request) -> Self {
        Self {
            index: request.index,
            seq: request.seq,
            request_digest: request.digest(),
        }
    }
}

/// How an unknown outcome was settled by the reopened log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resolution {
    /// The record is in the log: it committed.
    Committed(Receipt),
    /// The record is not in the log and the reopen's fence makes it impossible for it to appear.
    NotCommitted,
}

/// The committer: one lane, one writer, one append at a time.
pub struct Committer<S: MetadataStore, D: DependencyView> {
    writer: LaneWriter<S>,
    deps: D,
    policy: BatchPolicy,
}

fn owned(request: &Request) -> OwnedRecord {
    OwnedRecord {
        index: request.index,
        seq: request.seq,
        op: if request.delete {
            RecordOp::Delete
        } else {
            RecordOp::Put
        },
        payload: if request.delete {
            OwnedPayload::None
        } else {
            OwnedPayload::Inline(request.body.clone())
        },
    }
}

impl<S: MetadataStore, D: DependencyView> Committer<S, D> {
    /// Commit through `writer`, which must be an established writer of its lane.
    pub fn new(writer: LaneWriter<S>, deps: D, policy: BatchPolicy) -> Result<Self, LaneError> {
        Batcher::new(policy)?;
        Ok(Self {
            writer,
            deps,
            policy,
        })
    }

    /// Whether an earlier failure poisoned the lane writer: every later call is refused until
    /// [`Committer::recover`].
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.writer.is_poisoned()
    }

    /// The writer, for stats and membership.
    #[must_use]
    pub fn writer(&self) -> &LaneWriter<S> {
        &self.writer
    }

    /// Commit `requests` in order, one outcome per request, same order. An empty call makes no
    /// request of any kind.
    pub async fn commit(&mut self, requests: Vec<Request>) -> Vec<Outcome> {
        let mut out: Vec<Option<Outcome>> = vec![None; requests.len()];
        let mut batcher = match Batcher::new(self.policy) {
            Ok(b) => b,
            Err(_) => return vec![Outcome::Rejected(Reject::BadBody); requests.len()],
        };
        let mut batches = Vec::new();
        let mut seen: BTreeMap<(Id, u64), usize> = BTreeMap::new();
        let mut order: Vec<usize> = Vec::new();
        let mut refreshed = false;
        for (i, request) in requests.iter().enumerate() {
            if self.writer.is_poisoned() {
                out[i] = Some(Outcome::Rejected(Reject::Poisoned));
                continue;
            }
            if request.delete == !request.body.is_empty()
                || request.body.len() > MAX_INLINE_RECORD_BYTES
            {
                out[i] = Some(Outcome::Rejected(Reject::BadBody));
                continue;
            }
            if let Err(stale) = self.deps.check(&request.index, &request.deps) {
                out[i] = Some(Outcome::Rejected(Reject::Stale(stale)));
                continue;
            }
            if seen.insert((request.index, request.seq), i).is_some() {
                out[i] = Some(Outcome::Rejected(Reject::Duplicate));
                continue;
            }
            if self.writer.is_sealed(&request.index) {
                out[i] = Some(Outcome::Rejected(Reject::NotAdmitted));
                continue;
            }
            if self.writer.members().binary_search(&request.index).is_err() {
                // A joiner this writer has not seen: one descriptor re-read per call, however many
                // unknown indexes the call names.
                if !refreshed {
                    refreshed = true;
                    if self.writer.refresh_membership().await.is_err() {
                        out[i] = Some(Outcome::Rejected(Reject::Store));
                        continue;
                    }
                }
                if self.writer.members().binary_search(&request.index).is_err()
                    || self.writer.is_sealed(&request.index)
                {
                    out[i] = Some(Outcome::Rejected(Reject::NotAdmitted));
                    continue;
                }
            }
            match batcher.push(owned(request), Duration::ZERO) {
                Ok(closed) => batches.extend(closed),
                Err(_) => {
                    out[i] = Some(Outcome::Rejected(Reject::BadBody));
                    continue;
                }
            }
            order.push(i);
        }
        batches.extend(batcher.flush());
        // Batches hold the admitted records in arrival order, so they consume `order` front to back.
        let mut next = 0usize;
        let mut stopped = false;
        for batch in batches {
            let members: Vec<usize> = order[next..next + batch.records.len()].to_vec();
            next += batch.records.len();
            if stopped {
                for &i in &members {
                    out[i] = Some(Outcome::NotAttempted);
                }
                continue;
            }
            let result = self.writer.append(&batch.specs()).await;
            match result {
                Ok(ack) => {
                    for (position, &i) in members.iter().enumerate() {
                        let request = &requests[i];
                        out[i] = Some(Outcome::Committed(Receipt {
                            index: request.index,
                            seq: request.seq,
                            request_digest: request.digest(),
                            activation: self.writer.id().activation,
                            ordinal: ack.ordinal,
                            batch_position: position as u32,
                            protocol: Protocol::FencedSlot,
                        }));
                    }
                }
                Err(error) => {
                    stopped = true;
                    let outcome = match error {
                        LaneError::Unknown { ordinal } => Outcome::Unknown { ordinal },
                        LaneError::Fenced => Outcome::Rejected(Reject::Fenced),
                        LaneError::Poisoned => Outcome::Rejected(Reject::Poisoned),
                        LaneError::NotMember(_) | LaneError::Sealed(_) => {
                            Outcome::Rejected(Reject::NotAdmitted)
                        }
                        LaneError::Invalid(_) | LaneError::Capacity | LaneError::Format(_) => {
                            Outcome::Rejected(Reject::BadBody)
                        }
                        _ => Outcome::Rejected(Reject::Store),
                    };
                    for &i in &members {
                        out[i] = Some(outcome);
                    }
                }
            }
        }
        out.into_iter()
            .map(|o| o.unwrap_or(Outcome::NotAttempted))
            .collect()
    }

    /// Settle unknown outcomes by reopening the lane from `from` (the durable cover): the reopen
    /// fences the log's end, so a slot that was in flight can no longer appear, and the replay shows
    /// which `pending` records are in it. `from` must be the durable cover: a record in a slot below
    /// it is not seen and reads as `NotCommitted`. Callers must keep (index, seq) unique across
    /// calls; when a pair repeats, the one whose digest matches is reported. Returns the new committer, the open report and one
    /// [`Resolution`] per pending record, in order.
    pub async fn recover(
        store: Arc<S>,
        layout: Layout,
        id: LaneId,
        from: u64,
        deps: D,
        policy: BatchPolicy,
        pending: &[Pending],
    ) -> Result<(Self, OpenReport, Vec<Resolution>), LaneError> {
        let mut found: BTreeMap<(Id, u64), Vec<Found>> = BTreeMap::new();
        let wanted: BTreeMap<(Id, u64), usize> = pending
            .iter()
            .enumerate()
            .map(|(i, p)| ((p.index, p.seq), i))
            .collect();
        let activation = id.activation;
        let (writer, report) = LaneWriter::open(
            Arc::clone(&store),
            layout,
            id,
            from,
            |view: &SlotView<'_>| {
                if view.kind != SlotKind::Data {
                    return Ok(());
                }
                for (position, record) in view.records().enumerate() {
                    if !matches!(record.op, RecordOp::Put | RecordOp::Delete) {
                        continue;
                    }
                    if wanted.contains_key(&(record.index, record.seq)) {
                        let (delete, body): (bool, &[u8]) = match record.payload {
                            PayloadRef::None => (true, &[]),
                            PayloadRef::Inline(b) => (false, b),
                            PayloadRef::External { .. } => continue,
                        };
                        found.entry((record.index, record.seq)).or_default().push((
                            view.position.ordinal,
                            position as u32,
                            digest_of(&record.index, record.seq, delete, body),
                        ));
                    }
                }
                Ok(())
            },
        )
        .await?;
        let resolutions = pending
            .iter()
            .map(|p| {
                match found
                    .get(&(p.index, p.seq))
                    .and_then(|all| all.iter().find(|f| f.2 == p.request_digest))
                {
                    Some(&(ordinal, batch_position, digest)) => Resolution::Committed(Receipt {
                        index: p.index,
                        seq: p.seq,
                        request_digest: digest,
                        activation,
                        ordinal,
                        batch_position,
                        protocol: Protocol::FencedSlot,
                    }),
                    _ => Resolution::NotCommitted,
                }
            })
            .collect();
        Ok((Self::new(writer, deps, policy)?, report, resolutions))
    }
}
