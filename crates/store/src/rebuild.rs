//! Recovering a serving engine: recovery's records replayed into one.
//!
//! The committed log carries mutation batches (`hsearch_engine::encode_batch`); this replays the
//! ones after the base into an engine through the same preparation a live write meets, so the
//! documents, versions and request identities it holds are the ones the log implies. A record that
//! does not decode, or that the engine refuses, stops recovery with the sequence it failed at:
//! nothing is skipped.
use hsearch_engine::{Engine, Error as EngineError, Receipt, Schema, apply_batch};
use hsearch_object::{LargeObjectWriter, MetadataStore};

use crate::publish::{Dependencies, Publisher};
use crate::recover::{NotReady, Record, Recovered, Target, recover};

/// Why an engine could not be rebuilt.
#[derive(Debug, Eq, PartialEq)]
pub enum RebuildError {
    /// Recovery refused readiness; the engine was not rebuilt past what it applied.
    NotReady(NotReady),
    /// The engine refused the batch of this sequence (a body that does not decode included).
    Engine {
        /// The index sequence of the record.
        seq: u64,
        /// The engine's refusal.
        error: EngineError,
    },
    /// The head names a base, and this rebuild only replays onto an empty engine: serving the
    /// result would silently miss everything the base holds.
    BaseNotOpened,
    /// A record deletes a whole sequence slot rather than carrying a batch: batches carry their
    /// own document deletes, so this log is not one this rebuild understands.
    Unsupported(u64),
}

/// A rebuilt engine's account of what it replayed.
#[derive(Debug, Eq, PartialEq)]
pub struct Rebuilt {
    /// What recovery reached.
    pub recovered: Recovered,
    /// The receipts of each replayed batch, in sequence order.
    pub receipts: Vec<(u64, Vec<Receipt>)>,
}

/// Recover `target` and replay its records into an empty `engine`, which the caller discards on
/// any error (a refused or partial apply can leave it poisoned). Only batches the engine accepted
/// belong in the log, since a replayed refusal stops recovery for good. A head that names a base
/// is refused until M06.12 opens the engine on it. The receipts of every replayed batch are held,
/// bounded by the caller's record limit.
pub fn rebuild<S, D>(
    publisher: &Publisher<S>,
    target: &Target<'_>,
    dependencies: &D,
    schema: &Schema,
    engine: &Engine<'_>,
) -> Result<Rebuilt, RebuildError>
where
    S: MetadataStore + LargeObjectWriter + 'static,
    D: Dependencies,
{
    let mut receipts = Vec::new();
    let mut failure = None;
    let result = recover(publisher, target, dependencies, |r: &Record<'_>| {
        if r.delete {
            failure = Some(RebuildError::Unsupported(r.seq));
            return Err(());
        }
        match apply_batch(schema, engine, r.body) {
            Ok(done) => {
                receipts.push((r.seq, done));
                Ok(())
            }
            Err(error) => {
                failure = Some(RebuildError::Engine { seq: r.seq, error });
                Err(())
            }
        }
    });
    match result {
        Ok(recovered) if recovered.base.is_some() => Err(RebuildError::BaseNotOpened),
        Ok(recovered) => Ok(Rebuilt {
            recovered,
            receipts,
        }),
        Err(NotReady::Apply) => Err(failure.unwrap_or(RebuildError::NotReady(NotReady::Apply))),
        Err(other) => Err(RebuildError::NotReady(other)),
    }
}
