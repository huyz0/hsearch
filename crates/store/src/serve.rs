//! A serving writer: one index's batches validated, committed to the lane log, then applied.
//!
//! The log carries only batches the engine accepted, because a replayed refusal stops recovery for
//! good. So a batch is prepared against the schema and admitted by the engine
//! ([`Engine::admit`]) first, with nothing written; only then is it committed, and only a
//! committed batch is applied. Writes are serialised (`&mut self`), so the state the batch was
//! admitted against is the state it is applied to.
//!
//! **Nothing else may use the engine while a `Serving` writes to it.** The guarantee that an
//! admitted batch is applied rests on the engine's state not changing between the two calls;
//! a schema evolution, a direct `apply` or a native flush that crosses the segment bound in that
//! window makes the apply refuse a batch that is already in the log.
//!
//! What can still go wrong after the commit is a native failure inside the apply, or a segment
//! bound crossed by a flush in between. The batch is durable but the live engine did not take it, so
//! the writer is [`ServeError::Broken`] and refuses everything until the index is recovered from
//! the log (`recover_writer`), which replays it. An outcome the store could not settle
//! ([`ServeError::Unknown`]) breaks the writer the same way: the next recovery shows whether the
//! batch is in the log.
use crate::cover::SharedCover;
use crate::log::Log;
use crate::pointer::BaseSource;
use hsearch_commit::{Dependencies, Outcome, Receipt as Committed, Reject, Request};
use hsearch_engine::{
    Checked, Engine, Error as EngineError, LocalFiles, Mutation, PreparedBatch, Receipt, Schema,
    Snapshot, encode_batch,
};
use hsearch_format::{CommitToken, Digest, Id, ManifestRoot};
use hsearch_index::barrier::{Gen, Readers, Refuse, Replay};
use hsearch_lane::{LaneId, block_on, read_head};
use hsearch_object::{LargeObjectWriter, MetadataStore};
use std::sync::Arc;

use crate::payload::{self, PayloadSink};
use crate::publish::{self, BaseSpec, Publisher};
use crate::recover::Recovered;

/// Why a write did not complete.
#[derive(Debug, Eq, PartialEq)]
pub enum ServeError {
    /// The schema or the engine refused the batch; nothing was written and the writer is usable.
    Refused(EngineError),
    /// The store certainly did not take the record; nothing was written and the writer is usable
    /// (unless the lane writer is poisoned, which the next call reports as `Rejected(Poisoned)`).
    Rejected(Reject),
    /// The record may or may not be in the log. The writer is broken until recovered.
    Unknown {
        /// The slot that was being written.
        ordinal: u64,
    },
    /// The batch is in the log but the live engine did not take it, or an earlier failure broke
    /// the writer: recover the index from the log.
    Broken,
}

/// What a completed write returns: the engine's receipts, and where the record is in the log
/// (none for a request the engine had already applied, which is not written again).
#[derive(Debug, PartialEq)]
pub struct Written {
    /// The receipts, one per mutation.
    pub receipts: Vec<Receipt>,
    /// The log's receipt for the record.
    pub committed: Option<Committed>,
}

/// One batch of a chain: the request identity it is logged under and its mutations.
pub struct ChainBatch<'a> {
    /// The request identity, unique among the batches the engine has applied.
    pub request: &'a str,
    /// The mutations, versioned as they must be applied.
    pub mutations: &'a [Mutation<'a>],
}

/// Why a chain stopped before its last batch.
#[derive(Debug, Eq, PartialEq)]
pub enum ChainStop {
    /// The batch after the ones written was refused; nothing was written for it or any later one.
    Refused(EngineError),
    /// The batch after the ones written was applied before; it is not written again, and nothing
    /// was written for any later one.
    Recognised,
    /// The batch after the ones written is too large for a shared commit (its record would be an
    /// external payload): write it alone with [`Serving::write`]. Nothing was written for it or
    /// any later one.
    Alone,
    /// The write failed; see [`ServeError`] for what is certain about the records.
    Failed(ServeError),
}

/// What [`Serving::write_chain`] wrote.
#[derive(Debug, Default, PartialEq)]
pub struct Chained {
    /// The receipts of the leading batches that were written and applied, one list per batch.
    pub receipts: Vec<Vec<Receipt>>,
    /// Why the batches after those were not written, if they were not.
    pub stop: Option<ChainStop>,
}

/// One index's serving writer.
pub struct Serving<L: Log> {
    committer: L,
    index: Id,
    next: u64,
    deps: Dependencies,
    broken: bool,
    /// The applied prefix, which is what a published base may cover.
    replay: Replay,
    /// One past the lane slot of the last record committed, the lane cover of a base.
    cover: u64,
    /// The base this writer last published (or recovered on): the next one replaces it.
    base: Option<ManifestRoot>,
    /// The readers queries run on: the committed watermark and the published snapshot.
    readers: Arc<Readers<Snapshot>>,
    /// Where a batch too large for a record is written first; without one it is refused.
    payloads: Option<Box<dyn PayloadSink>>,
    /// The lane's pinned set when the lane is shared: every record this writer commits pins it.
    pins: Option<Arc<SharedCover>>,
}

/// Generations a query may hold at once besides the newest. The engine allows four open
/// snapshots in all: this and the newest use three, leaving one for a snapshot opened before the
/// barrier refuses it (an engine at its limit is [`RefreshError::Engine`], not `Refused`).
pub const MAX_PINNED_READERS: usize = 2;

/// Why a refresh published nothing. The previously published reader still serves.
#[derive(Debug)]
pub enum RefreshError {
    /// The applied prefix is not a barrier cut (a batch is in flight or the cursor failed).
    Cut,
    /// The engine could not open a reader (a snapshot limit, or a native failure).
    Engine(EngineError),
    /// The reader was not published: it covers slots not committed, fewer than the one serving,
    /// or too many older generations are still held by queries.
    Refused(Refuse),
}

/// Where a base is published and what it is made under.
pub struct BaseTarget<'a> {
    /// The lane whose structural head will name the base.
    pub lane: &'a LaneId,
    /// The shard.
    pub shard: Id,
    /// The shard's incarnation.
    pub incarnation: u64,
    /// The native format label recorded in the root.
    pub engine_format: &'a str,
    /// The mapping the files were written under.
    pub mapping: Digest,
    /// The key envelope the files are protected under.
    pub crypto_envelope: Id,
    /// Whether the base is named in the lane head or in the shard's own pointer.
    pub base: BaseSource,
}

/// Why a base could not be published.
#[derive(Debug)]
pub enum BaseError {
    /// The writer must be recovered first.
    Broken,
    /// No batch has been committed since this writer began: there is nothing to cover.
    Nothing,
    /// The engine could not commit or write the list.
    Engine(EngineError),
    /// The applied prefix is not a barrier cut (a batch is in flight or the cursor failed).
    Cut,
    /// The head could not be read.
    Head(String),
    /// Publishing or activating failed. [`publish::Error::Swept`] means a collection passed the
    /// base's generation: the previous base is no longer the one to replace.
    Publish(publish::Error),
}

impl<L: Log> Serving<L> {
    /// Serve `index` through `committer`, whose lane the index belongs to. `next` is the index's
    /// next sequence: `Recovered::applied` after a recovery, 0 for a new index. `deps` is what
    /// every record is prepared against; it is fixed for the writer's life, so after a dependency
    /// changes every write is `Rejected(Stale)` until the writer is rebuilt.
    pub fn new(committer: L, index: Id, next: u64, deps: Dependencies) -> Self {
        let readers = Arc::new(Readers::new(MAX_PINNED_READERS));
        // Everything before `next` is committed and applied; the watermark starts there.
        let _ = readers.commit(next);
        Self {
            committer,
            index,
            next,
            deps,
            broken: false,
            replay: Replay::new(next),
            cover: 0,
            base: None,
            readers,
            payloads: None,
            pins: None,
        }
    }

    /// Write batches too large for a lane record as external payload objects through `sink`.
    #[must_use]
    pub fn with_payloads(mut self, sink: Box<dyn PayloadSink>) -> Self {
        self.payloads = Some(sink);
        self
    }

    /// Serve `index` again after a recovery: the next sequence is what was applied, the base it
    /// was recovered on is the one the next publication replaces, and the lane cover starts at the
    /// first absent position recovery found (zero when recovery followed a seal to another lane,
    /// where the cover is known only after the first write). A writer made this way can publish a
    /// base before it writes.
    pub fn recovered(committer: L, index: Id, recovered: &Recovered, deps: Dependencies) -> Self {
        let mut writer = Self::new(committer, index, recovered.applied, deps);
        writer.base.clone_from(&recovered.base);
        if recovered.followed == 0 {
            writer.cover = recovered.tail;
        }
        writer
    }

    /// Pin the lane's cover for this index from slot `from` (where recovery started) until a base
    /// covering what it wrote is published, and on every record it commits after.
    #[must_use]
    pub fn with_cover(mut self, pins: Arc<SharedCover>, from: u64) -> Self {
        pins.pin(self.index, from);
        self.pins = Some(pins);
        self
    }

    fn note_write(&self, slot: u64) {
        if let Some(pins) = &self.pins {
            pins.wrote(self.index, slot);
        }
    }

    /// The sequence the next record will take.
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next
    }

    /// Whether the writer must be replaced by a recovery.
    #[must_use]
    pub fn is_broken(&self) -> bool {
        self.broken || self.committer.is_poisoned()
    }

    /// The published reader, pinned until the handle is dropped: queries run on it, and the same
    /// handle serves the fetch round. `None` before the first [`Serving::refresh`].
    #[must_use]
    pub fn reader(&self) -> Option<Arc<Gen<Snapshot>>> {
        self.readers.pin()
    }

    /// The barrier's readers, shared: other threads pin the serving reader from it while this
    /// writer keeps refreshing it.
    #[must_use]
    pub fn readers(&self) -> Arc<Readers<Snapshot>> {
        Arc::clone(&self.readers)
    }

    /// Prepare every later record against `deps`: after the schema (or another dependency) the
    /// committer's view checks has moved on and the engine has followed it.
    pub fn set_deps(&mut self, deps: Dependencies) {
        self.deps = deps;
    }

    /// Whether a refresh would publish a reader covering more than the one serving.
    #[must_use]
    pub fn refresh_due(&self) -> bool {
        self.readers.refresh_due(self.replay.applied())
    }

    /// Publish a reader over every batch applied so far, if that is more than the one serving.
    /// Returns the reader serving afterwards. The engine is not written to meanwhile (`&mut
    /// self`), so the snapshot covers exactly the barrier cut. Any failure leaves the previously
    /// published reader serving, and the next refresh tries again. A broken writer can still
    /// refresh: a reader covers only batches that were applied, so it stays consistent, and only
    /// a failed cursor ([`RefreshError::Cut`]) stops it.
    ///
    /// # Errors
    /// [`RefreshError`] naming the step that failed; nothing was published.
    pub fn refresh(
        &mut self,
        engine: &Engine<'_>,
    ) -> Result<Option<Arc<Gen<Snapshot>>>, RefreshError> {
        if !self.refresh_due() {
            return Ok(self.readers.pin());
        }
        self.publish_now(engine)
    }

    /// Publish a reader over the engine as it is, whether or not more has been applied since the
    /// last one: readers opened after a schema change must know the schema.
    ///
    /// # Errors
    /// [`RefreshError`] naming the step that failed; nothing was published.
    pub fn republish(&mut self, engine: &Engine<'_>) -> Result<(), RefreshError> {
        self.publish_now(engine).map(|_| ())
    }

    fn publish_now(
        &mut self,
        engine: &Engine<'_>,
    ) -> Result<Option<Arc<Gen<Snapshot>>>, RefreshError> {
        let cut = self.replay.cut().map_err(|_| RefreshError::Cut)?;
        let snapshot = engine.refresh().map_err(RefreshError::Engine)?;
        self.readers
            .publish(snapshot, cut)
            .map(Some)
            .map_err(|refused| RefreshError::Refused(refused.reason))
    }

    /// Publish a reader over the engine as it is when none has been published: an index with
    /// nothing in it can be searched and answers with no hits. Does nothing if a reader serves.
    ///
    /// # Errors
    /// [`RefreshError`] naming the step that failed; nothing was published.
    pub fn publish_empty(&mut self, engine: &Engine<'_>) -> Result<(), RefreshError> {
        if self.readers.pin().is_some() {
            return Ok(());
        }
        let cut = self.replay.cut().map_err(|_| RefreshError::Cut)?;
        let snapshot = engine.refresh().map_err(RefreshError::Engine)?;
        self.readers
            .publish(snapshot, cut)
            .map(|_| ())
            .map_err(|refused| RefreshError::Refused(refused.reason))
    }

    /// Prepare, admit, commit and apply one batch.
    pub fn write(
        &mut self,
        schema: &Schema,
        engine: &Engine<'_>,
        request: &str,
        mutations: &[Mutation<'_>],
    ) -> Result<Written, ServeError> {
        if self.broken {
            return Err(ServeError::Broken);
        }
        let prepared = schema
            .prepare(request, mutations)
            .map_err(ServeError::Refused)?;
        let admitted = engine.admit(&prepared);
        // An engine that is poisoned will refuse everything: that is a writer to replace, not a
        // refusal of this batch.
        if matches!(admitted, Err(EngineError::Poisoned | EngineError::Native)) {
            self.broken = true;
            return Err(ServeError::Broken);
        }
        match admitted.map_err(ServeError::Refused)? {
            Checked::Recognised(receipts) => {
                return Ok(Written {
                    receipts,
                    committed: None,
                });
            }
            Checked::Fresh => {}
        }
        let body = encode_batch(request, mutations).map_err(ServeError::Refused)?;
        let record = Request {
            index: self.index,
            seq: self.next,
            body,
            delete: false,
            deps: self.deps,
        };
        let outcome = payload::commit(&mut self.committer, self.payloads.as_deref(), record)?;
        let committed = match outcome {
            Outcome::Committed(receipt) => receipt,
            // A taken slot position may be our own accepted slot (see `Reject::Fenced`), so the
            // record may be in the log: only a recovery settles it.
            Outcome::Rejected(Reject::Fenced) => {
                self.broken = true;
                return Err(ServeError::Broken);
            }
            Outcome::Rejected(reason) => return Err(ServeError::Rejected(reason)),
            Outcome::NotAttempted => return Err(ServeError::Rejected(Reject::Poisoned)),
            Outcome::Unknown { ordinal } => {
                self.broken = true;
                return Err(ServeError::Unknown { ordinal });
            }
        };
        self.next += 1;
        self.cover = committed.ordinal + 1;
        self.note_write(committed.ordinal);
        // The watermark only rises (`next` only does), so this cannot be refused.
        let _ = self.readers.commit(self.next);
        // The record is durable: the cursor follows the engine through the apply.
        let in_flight = self.replay.begin(committed.seq).is_ok();
        match engine.apply(prepared) {
            Ok(receipts) if in_flight && self.replay.done(committed.seq).is_ok() => Ok(Written {
                receipts,
                committed: Some(committed),
            }),
            _ => {
                self.replay.fail();
                self.broken = true;
                Err(ServeError::Broken)
            }
        }
    }

    /// Write a run of batches as one commit and apply them in order: they share a lane slot (or
    /// as few as the slot bounds allow), so the cost is a request per slot and not per batch. The
    /// engine decides all of them together ([`Engine::admit_chain`]) before anything is logged,
    /// so a logged batch is one the engine will take when it is applied after the ones before it.
    ///
    /// Batches are written up to the first one that is refused, already applied, or whose
    /// records could not be written; that one and the rest are not written, and the writer says
    /// why in `stop`. Each record takes the next sequence, so a caller must version batch `i`
    /// as if it were record `next_seq() + i`.
    pub fn write_chain(
        &mut self,
        schema: &Schema,
        engine: &Engine<'_>,
        batches: &[ChainBatch<'_>],
    ) -> Chained {
        let mut out = Chained::default();
        if self.broken {
            out.stop = Some(ChainStop::Failed(ServeError::Broken));
            return out;
        }
        let mut prepared = Vec::with_capacity(batches.len());
        let mut bodies = Vec::with_capacity(batches.len());
        for batch in batches {
            let made = schema
                .prepare(batch.request, batch.mutations)
                .and_then(|p| encode_batch(batch.request, batch.mutations).map(|b| (p, b)));
            match made {
                Ok((_, body)) if body.len() > hsearch_format::MAX_INLINE_RECORD_BYTES => {
                    out.stop = Some(ChainStop::Alone);
                    break;
                }
                Ok((p, body)) => {
                    prepared.push(p);
                    bodies.push(body);
                }
                Err(e) => {
                    out.stop = Some(ChainStop::Refused(e));
                    break;
                }
            }
        }
        let decisions = {
            let refs: Vec<&PreparedBatch> = prepared.iter().collect();
            engine.admit_chain(&refs)
        };
        let decisions = match decisions {
            Ok(decisions) => decisions,
            Err(_) => {
                // The engine is poisoned: that is a writer to replace, not a refusal.
                self.broken = true;
                out.stop = Some(ChainStop::Failed(ServeError::Broken));
                return out;
            }
        };
        let mut fresh = prepared.len();
        for (i, decision) in decisions.into_iter().enumerate() {
            let stop = match decision {
                Ok(Checked::Fresh) => continue,
                Ok(Checked::Recognised(_)) => ChainStop::Recognised,
                Err(EngineError::Poisoned | EngineError::Native) => {
                    self.broken = true;
                    ChainStop::Failed(ServeError::Broken)
                }
                Err(e) => ChainStop::Refused(e),
            };
            fresh = i;
            out.stop = Some(stop);
            break;
        }
        prepared.truncate(fresh);
        bodies.truncate(fresh);
        if prepared.is_empty() {
            return out;
        }
        let requests: Vec<Request> = bodies
            .into_iter()
            .enumerate()
            .map(|(j, body)| Request {
                index: self.index,
                seq: self.next + j as u64,
                body,
                delete: false,
                deps: self.deps,
            })
            .collect();
        let outcomes = self.committer.commit(requests);
        let mut pending = prepared.into_iter();
        let mut failure: Option<ServeError> = None;
        for outcome in outcomes {
            let Some(batch) = pending.next() else { break };
            if failure.is_some() {
                // A record after a failed one that is in the log leaves the engine behind it.
                if matches!(outcome, Outcome::Committed(_) | Outcome::Unknown { .. }) {
                    self.broken = true;
                    failure = Some(ServeError::Broken);
                }
                continue;
            }
            match outcome {
                Outcome::Committed(committed) => {
                    self.next += 1;
                    self.cover = committed.ordinal + 1;
                    self.note_write(committed.ordinal);
                    // The watermark only rises (`next` only does), so this cannot be refused.
                    let _ = self.readers.commit(self.next);
                    let in_flight = self.replay.begin(committed.seq).is_ok();
                    match engine.apply(batch) {
                        Ok(receipts) if in_flight && self.replay.done(committed.seq).is_ok() => {
                            out.receipts.push(receipts);
                        }
                        _ => {
                            self.replay.fail();
                            self.broken = true;
                            failure = Some(ServeError::Broken);
                        }
                    }
                }
                // A taken slot position may be our own accepted slot (see `Reject::Fenced`).
                Outcome::Rejected(Reject::Fenced) => {
                    self.broken = true;
                    failure = Some(ServeError::Broken);
                }
                Outcome::Rejected(reason) => failure = Some(ServeError::Rejected(reason)),
                Outcome::NotAttempted => failure = Some(ServeError::Rejected(Reject::Poisoned)),
                Outcome::Unknown { ordinal } => {
                    self.broken = true;
                    failure = Some(ServeError::Unknown { ordinal });
                }
            }
        }
        if let Some(failure) = failure {
            out.stop = Some(ChainStop::Failed(failure));
        }
        out
    }

    /// Commit the engine's files and publish them with the tombstone list as a base covering every
    /// batch applied so far, then name it in the lane's structural head. The engine is not
    /// written to meanwhile (`&mut self`). A collection boundary above the base's generation
    /// refuses the activation ([`publish::Error::Swept`]) and nothing names the new objects.
    ///
    /// The base replaced is the one this writer last published or recovered on. Nothing checks
    /// that the head still names it, so a compaction that replaced it in the meantime is not seen
    /// here (the publication generation could then fail to rise past the compacted one).
    ///
    /// A failed publish leaves the writer's base as it was, so a retry computes the same
    /// generation and can fail the same way after [`publish::Error::Swept`]; each attempt uploads
    /// its files again. The caller bounds its attempts and rebuilds the writer after a `Swept`.
    ///
    /// The lane cover is one past the last record this writer committed (or recovery's first
    /// absent position), which is only the right cover for a lane this index owns alone.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_base<P>(
        &mut self,
        engine: &Engine<'_>,
        local: &LocalFiles,
        publisher: &Publisher<P>,
        target: &BaseTarget<'_>,
        attempt: CommitToken,
        dependencies: &dyn publish::Dependencies,
    ) -> Result<publish::Published, BaseError>
    where
        P: MetadataStore + LargeObjectWriter + 'static,
    {
        if self.is_broken() {
            return Err(BaseError::Broken);
        }
        if self.cover == 0 {
            return Err(BaseError::Nothing);
        }
        let cut = self.replay.cut().map_err(|_| BaseError::Cut)?;
        let checkpoint = engine.commit().map_err(BaseError::Engine)?;
        local
            .write_tombstones(&checkpoint)
            .map_err(BaseError::Engine)?;
        let names = local.names().map_err(BaseError::Engine)?;
        let spec = BaseSpec {
            index: self.index,
            shard: target.shard,
            incarnation: target.incarnation,
            engine_format: target.engine_format,
            mapping: target.mapping,
            crypto_envelope: target.crypto_envelope,
            cut,
            committed: self.next,
            previous: self.base.as_ref(),
            attempt,
        };
        let published = publisher
            .publish(local, &names, &spec, dependencies)
            .map_err(BaseError::Publish)?;
        let (head, version) = block_on(read_head(
            publisher.store(),
            publisher.layout(),
            target.lane,
        ))
        .map_err(|e| BaseError::Head(format!("{e:?}")))?;
        if target.base == BaseSource::Pointer {
            // The pointer read here is the one the compare-and-swap names: a base published since
            // by another writer of this shard makes the write lose.
            let current = publisher.read_pointer(&spec).map_err(BaseError::Publish)?;
            // Only the base this writer built on may be replaced, by a writer of the live lane
            // activation: the fresh read alone would let a second builder overwrite the first.
            let held = self
                .base
                .as_ref()
                .map(|b| publish::manifest_id(&spec.index, &spec.shard, &b.token));
            if current.as_ref().map(|(p, _)| p.manifest) != held
                || head.activation != target.lane.activation
            {
                return Err(BaseError::Publish(publish::Error::Lost));
            }
            publisher
                .activate_pointer(
                    current.as_ref(),
                    &publish::LogStart {
                        lane: target.lane,
                        // The head's cover may have passed this index's last record (an earlier
                        // base covered it and unpinned the index): nothing of it lies below.
                        lane_cover: self.cover.max(head.cover),
                        head_cover: head.cover,
                    },
                    &published,
                    &spec,
                    dependencies,
                )
                .map_err(BaseError::Publish)?;
            self.base = Some(published.root.clone());
            if let Some(pins) = &self.pins {
                pins.published(&self.index, self.cover);
            }
            return Ok(published);
        }
        publisher
            .activate(
                &publish::HeadRead {
                    lane: target.lane,
                    head: &head,
                    version: &version,
                },
                &published,
                &spec,
                self.cover,
                dependencies,
            )
            .map_err(BaseError::Publish)?;
        self.base = Some(published.root.clone());
        Ok(published)
    }
}
