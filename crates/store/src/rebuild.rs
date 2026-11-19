//! Recovering a serving engine: recovery's records replayed into one.
//!
//! The committed log carries mutation batches (`hsearch_engine::encode_batch`); this replays the
//! ones after the base into an engine through the same preparation a live write meets, so the
//! documents, versions and request identities it holds are the ones the log implies. A record that
//! does not decode, or that the engine refuses, stops recovery with the sequence it failed at:
//! nothing is skipped.
use std::path::Path;

use hsearch_engine::{
    Engine, Error as EngineError, MergeMode, Receipt, Schema, TOMBSTONES_FILE, apply_batch,
    with_local_engine, with_recovered_local_engine,
};
use hsearch_format::{ManifestRoot, StructuralHead};
use hsearch_lane::{Error as LaneError, block_on, read_head};
use hsearch_object::{LargeObjectWriter, MetadataStore};

use crate::publish::{Dependencies, Error as PublishError, Publisher};
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
    /// The head moved to another base between opening the engine and replaying its tail: read
    /// the head again and recover again.
    BaseMoved,
    /// The base carries no tombstone list, so a writer cannot be reopened on it.
    MissingTombstones,
    /// The engine could not be opened on the base's files.
    Open(EngineError),
    /// Scratch space could not be prepared or a base file could not be fetched into it.
    Scratch(String),
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
    rebuild_onto(publisher, target, dependencies, schema, engine, None)
}

/// [`rebuild`] onto an engine opened on `opened`, the base the head named (none for an empty
/// engine). The head must still name that base when the tail has been replayed.
fn rebuild_onto<S, D>(
    publisher: &Publisher<S>,
    target: &Target<'_>,
    dependencies: &D,
    schema: &Schema,
    engine: &Engine<'_>,
    opened: Option<&ManifestRoot>,
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
        Ok(recovered) => match (recovered.base.as_ref(), opened) {
            (None, None) => Ok(Rebuilt {
                recovered,
                receipts,
            }),
            (Some(_), None) => Err(RebuildError::BaseNotOpened),
            (Some(a), Some(b)) if a.token == b.token => Ok(Rebuilt {
                recovered,
                receipts,
            }),
            _ => Err(RebuildError::BaseMoved),
        },
        Err(NotReady::Apply) => Err(failure.unwrap_or(RebuildError::NotReady(NotReady::Apply))),
        Err(other) => Err(RebuildError::NotReady(other)),
    }
}

/// Recover a serving writer with nothing local: read the head, and if it names a base fetch the
/// base's files (digest-checked) and tombstone list into `scratch` (emptied first), open the engine
/// on them, replay the committed tail onto it, and run `run` with the engine and what was replayed.
/// With no base the engine starts empty and the whole log replays. The engine is closed without
/// committing afterwards; any error means the scratch directory is to be discarded.
pub fn recover_writer<S, D, R>(
    publisher: &Publisher<S>,
    target: &Target<'_>,
    dependencies: &D,
    schema: &Schema,
    scratch: &Path,
    run: impl FnOnce(&Engine<'_>, &Rebuilt) -> R,
) -> Result<R, RebuildError>
where
    S: MetadataStore + LargeObjectWriter + 'static,
    D: Dependencies,
{
    let (head, _): (StructuralHead, _) = match block_on(read_head(
        publisher.store(),
        publisher.layout(),
        target.lane,
    )) {
        Ok(found) => found,
        Err(LaneError::NoHead) => return Err(RebuildError::NotReady(NotReady::NoHead)),
        Err(LaneError::Format(_)) => return Err(RebuildError::NotReady(NotReady::Corrupt)),
        Err(e) => return Err(RebuildError::NotReady(NotReady::Store(format!("{e:?}")))),
    };
    let Some(manifest) = head.checkpoint else {
        claim_scratch(scratch)?;
        return with_local_engine(scratch, schema.clone(), |engine, _| {
            rebuild_onto(publisher, target, dependencies, schema, engine, None)
                .map(|rebuilt| run(engine, &rebuilt))
        })
        .map_err(RebuildError::Open)?;
    };
    let root = publisher
        .verify_root(
            &target.index,
            &target.shard,
            target.incarnation,
            &manifest,
            dependencies,
        )
        .map_err(|e| {
            RebuildError::NotReady(match e {
                PublishError::Store(hsearch_object::Error::NotFound) => {
                    NotReady::Base(format!("{e:?}"))
                }
                PublishError::Store(_) => NotReady::Store(format!("{e:?}")),
                other => NotReady::Base(format!("{other:?}")),
            })
        })?;
    let entries = publisher
        .entries(&target.index, &target.shard, target.incarnation, &root)
        .map_err(|e| RebuildError::NotReady(NotReady::Base(format!("{e:?}"))))?;
    if !entries.iter().any(|e| e.name == TOMBSTONES_FILE) {
        return Err(RebuildError::MissingTombstones);
    }
    let scratch_error =
        |what: &str, e: &dyn std::fmt::Debug| RebuildError::Scratch(format!("{what}: {e:?}"));
    claim_scratch(scratch)?;
    if scratch.exists() {
        std::fs::remove_dir_all(scratch).map_err(|e| scratch_error("clear", &e))?;
    }
    std::fs::create_dir_all(scratch).map_err(|e| scratch_error("create", &e))?;
    for entry in &entries {
        publisher
            .fetch(&target.index, entry, scratch)
            .map_err(|e| match e {
                PublishError::Store(hsearch_object::Error::NotFound) => {
                    RebuildError::NotReady(NotReady::Base(format!("{e:?}")))
                }
                other => scratch_error(&entry.name, &other),
            })?;
    }
    with_recovered_local_engine(scratch, schema.clone(), MergeMode::Inline, |engine, _| {
        rebuild_onto(publisher, target, dependencies, schema, engine, Some(&root))
            .map(|rebuilt| run(engine, &rebuilt))
    })
    .map_err(|e| match e {
        EngineError::Checkpoint => RebuildError::Open(EngineError::Checkpoint),
        other => RebuildError::Open(other),
    })?
}

/// The marker file recording that a directory is recovery scratch: a sibling of the directory (so
/// the engine never sees it), named after it.
fn scratch_marker(scratch: &Path) -> Option<std::path::PathBuf> {
    let name = scratch.file_name()?.to_str()?;
    Some(scratch.with_file_name(format!("{name}.hsearch-scratch")))
}

/// Make `scratch` safe to empty: it is absent, empty, or marked as scratch by an earlier recovery.
/// Anything else (a data directory, a home directory, `/`) is refused untouched. On success the
/// marker is written, so the next recovery may reuse the directory.
fn claim_scratch(scratch: &Path) -> Result<(), RebuildError> {
    let refuse = |what: &str| RebuildError::Scratch(format!("{what}: {}", scratch.display()));
    let marker = scratch_marker(scratch).ok_or_else(|| refuse("not a usable scratch path"))?;
    if scratch.exists() {
        let empty = std::fs::read_dir(scratch)
            .map_err(|_| refuse("unreadable"))?
            .next()
            .is_none();
        if !empty && !marker.exists() {
            return Err(refuse("not a recovery scratch directory"));
        }
    }
    std::fs::write(&marker, b"hsearch recovery scratch\n").map_err(|_| refuse("marker"))
}
