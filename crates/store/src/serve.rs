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
use hsearch_commit::{
    Committer, Dependencies, DependencyView, Outcome, Receipt as Committed, Reject, Request,
};
use hsearch_engine::{
    Checked, Engine, Error as EngineError, Mutation, Receipt, Schema, encode_batch,
};
use hsearch_format::Id;
use hsearch_lane::block_on;
use hsearch_object::MetadataStore;

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

/// One index's serving writer.
pub struct Serving<S: MetadataStore, D: DependencyView> {
    committer: Committer<S, D>,
    index: Id,
    next: u64,
    deps: Dependencies,
    broken: bool,
}

impl<S: MetadataStore, D: DependencyView> Serving<S, D> {
    /// Serve `index` through `committer`, whose lane the index belongs to. `next` is the index's
    /// next sequence: `Recovered::applied` after a recovery, 0 for a new index. `deps` is what
    /// every record is prepared against; it is fixed for the writer's life, so after a dependency
    /// changes every write is `Rejected(Stale)` until the writer is rebuilt.
    pub fn new(committer: Committer<S, D>, index: Id, next: u64, deps: Dependencies) -> Self {
        Self {
            committer,
            index,
            next,
            deps,
            broken: false,
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
        let committed = match block_on(self.committer.commit(vec![record])).remove(0) {
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
        match engine.apply(prepared) {
            Ok(receipts) => Ok(Written {
                receipts,
                committed: Some(committed),
            }),
            Err(_) => {
                self.broken = true;
                Err(ServeError::Broken)
            }
        }
    }
}
