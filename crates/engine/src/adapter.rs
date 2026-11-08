//! Serialized admission around a caller-owned directory and native writer.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, atomic::AtomicUsize};

use lucene_index::buffered_updates::Term;
use lucene_index::document::Document;
use lucene_index::index_writer::IndexWriter;
use lucene_index::segment_info::LuceneVersion;
use lucene_search::directory_reader::DirectoryReader;

use crate::checkpoint::{Head, Lineage, NativeCommit, WriterLease};
use crate::directory::DirectoryAttempt;
use crate::schema::{MAX_DOCUMENTS, MAX_OPERATIONS, analyzer};
use crate::{
    CleanupStatus, Directory, EngineDirectory, Error, ReleaseStatus, Result, Schema, Value,
};

const VERSION: LuceneVersion = LuceneVersion {
    major: 10,
    minor: 5,
    bugfix: 0,
};
const MAX_BATCH: usize = 16;
const MAX_BATCH_BYTES: usize = 65_536;
const MAX_HISTORY_BYTES: usize = 262_144;

/// Caller-resolved mutation. The adapter never generates IDs or versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutation<'a> {
    /// Replace or create a whole document at a strictly newer logical version.
    Put {
        /// Stable logical identity.
        id: &'a str,
        /// Positive caller-resolved version.
        version: i64,
        /// Exactly the schema's fields, in canonical schema order.
        fields: &'a [(&'a str, Value<'a>)],
    },
    /// Create only when no live document exists, at a newer caller version.
    Create {
        /// Stable logical identity.
        id: &'a str,
        /// Positive caller-resolved version.
        version: i64,
        /// Exactly the schema's fields in canonical schema order.
        fields: &'a [(&'a str, Value<'a>)],
    },
    /// Remove a document, retaining a versioned tombstone for retries.
    Delete {
        /// Stable logical identity.
        id: &'a str,
        /// Positive caller-resolved version.
        version: i64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OwnedValue {
    String(String),
    Integer(i64),
    Boolean(bool),
    Date(i64),
}
impl OwnedValue {
    fn borrowed(&self) -> Value<'_> {
        match self {
            Self::String(s) => Value::String(s),
            Self::Integer(n) => Value::Integer(*n),
            Self::Boolean(b) => Value::Boolean(*b),
            Self::Date(n) => Value::Date(*n),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Operation {
    id: String,
    create_only: bool,
    pub(crate) version: i64,
    pub(crate) fields: Option<Vec<(String, OwnedValue)>>,
}
impl Operation {
    fn document(&self, schema: &Schema) -> Result<Option<Document>> {
        self.fields
            .as_ref()
            .map(|fields| {
                let values: Vec<_> = fields
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.borrowed()))
                    .collect();
                schema.document(&self.id, self.version, &values)
            })
            .transpose()
    }
}

/// Validated private preparation. Dropping it has no writer effects.
pub struct PreparedBatch {
    schema: Schema,
    request: String,
    operations: Vec<Operation>,
    documents: Vec<Option<Document>>,
    bytes: usize,
}

impl Schema {
    /// Prepare an entire bounded batch before native mutation or admission.
    /// Repeated identities within a batch are refused to keep receipts unambiguous.
    pub fn prepare(&self, request: &str, mutations: &[Mutation<'_>]) -> Result<PreparedBatch> {
        if request.is_empty()
            || request.len() > 256
            || mutations.is_empty()
            || mutations.len() > MAX_BATCH
        {
            return Err(Error::Limit);
        }
        let mut bytes = request.len();
        // Preflight the whole input before allocating canonical copies.
        for (i, mutation) in mutations.iter().enumerate() {
            let (id, version, fields) = match mutation {
                Mutation::Put {
                    id,
                    version,
                    fields,
                }
                | Mutation::Create {
                    id,
                    version,
                    fields,
                } => (*id, *version, Some(*fields)),
                Mutation::Delete { id, version } => (*id, *version, None),
            };
            if id.is_empty() || id.len() > 256 || version <= 0 {
                return Err(Error::Document);
            }
            if mutations[..i].iter().any(|m| match m {
                Mutation::Put { id: other, .. }
                | Mutation::Create { id: other, .. }
                | Mutation::Delete { id: other, .. } => *other == id,
            }) {
                return Err(Error::Conflict);
            }
            bytes = bytes.checked_add(id.len()).ok_or(Error::Limit)?;
            if let Some(fields) = fields {
                if fields.len() != self.fields.len() {
                    return Err(Error::Document);
                }
                for ((name, value), (declared, kind)) in fields.iter().zip(&self.fields) {
                    if *name != declared {
                        return Err(Error::Document);
                    }
                    crate::schema::validate(*kind, *value)?;
                    bytes = bytes
                        .checked_add(name.len())
                        .and_then(|n| {
                            n.checked_add(match value {
                                Value::String(s) => s.len(),
                                _ => 8,
                            })
                        })
                        .ok_or(Error::Limit)?;
                    if bytes > MAX_BATCH_BYTES {
                        return Err(Error::Limit);
                    }
                }
            }
            if bytes > MAX_BATCH_BYTES {
                return Err(Error::Limit);
            }
        }
        let operations: Vec<_> = mutations
            .iter()
            .map(|m| match m {
                Mutation::Put {
                    id,
                    version,
                    fields,
                }
                | Mutation::Create {
                    id,
                    version,
                    fields,
                } => Operation {
                    id: id.to_string(),
                    create_only: matches!(m, Mutation::Create { .. }),
                    version: *version,
                    fields: Some(
                        fields
                            .iter()
                            .map(|(n, v)| {
                                (
                                    n.to_string(),
                                    match v {
                                        Value::String(s) => OwnedValue::String(s.to_string()),
                                        Value::Integer(n) => OwnedValue::Integer(*n),
                                        Value::Boolean(b) => OwnedValue::Boolean(*b),
                                        Value::Date(n) => OwnedValue::Date(*n),
                                    },
                                )
                            })
                            .collect(),
                    ),
                },
                Mutation::Delete { id, version } => Operation {
                    id: id.to_string(),
                    create_only: false,
                    version: *version,
                    fields: None,
                },
            })
            .collect();
        let documents = operations
            .iter()
            .map(|op| op.document(self))
            .collect::<Result<_>>()?;
        Ok(PreparedBatch {
            schema: self.clone(),
            request: request.to_string(),
            operations,
            documents,
            bytes,
        })
    }
}

/// Native logical item outcome, independent of any HTTP response contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A previously absent or deleted identity became live.
    Created,
    /// A live document was replaced.
    Updated,
    /// A live document was deleted.
    Deleted,
    /// A missing identity was deleted, retaining its new versioned tombstone.
    NotFound,
}

/// Stable item result, returned in submission order, including missing deletes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// Caller identity, never a Lucene internal document number.
    pub id: String,
    /// Caller-resolved version.
    pub version: i64,
    /// Whether the mutation removed a live document.
    pub removed: bool,
    /// Original logical outcome, retained unchanged for an exact retry.
    pub outcome: Outcome,
}

#[derive(Clone)]
struct History {
    request: String,
    operations: Vec<Operation>,
    receipts: Vec<Receipt>,
}
#[derive(Clone, Default)]
struct State {
    current: BTreeMap<String, Operation>,
    history: Vec<History>,
    operations: usize,
    history_bytes: usize,
    physical_documents: usize,
    poisoned: bool,
}
/// Opaque in-process native-commit checkpoint. Not a persisted hsearch format.
/// A caller must retain this state or replay the original resolved history into
/// a fresh directory. Native files alone cannot reconstruct retry receipts.
#[derive(Clone)]
pub struct Checkpoint {
    schema: Schema,
    state: State,
    lineage: Arc<Lineage>,
    epoch: u64,
    native: NativeCommit,
}

/// Caller-owned-directory adapter with one writer and bounded nonblocking admission.
/// Only methods on this adapter may mutate its directory while it is open.
pub struct Engine<'d> {
    // Declaration order is part of WriterLease's implicit-drop invariant.
    writer: Mutex<IndexWriter<'d>>,
    lease: WriterLease,
    attempt: DirectoryAttempt<'d>,
    directory: &'d dyn Directory,
    snapshots: Arc<AtomicUsize>,
    schema: Schema,
    state: Mutex<State>,
}

// Keep declaration-order cleanup even when native close unwinds after the
// Engine has been destructured into locals.
struct Closing<'d> {
    lease: WriterLease,
    attempt: DirectoryAttempt<'d>,
}

struct Opening<'a, 'd> {
    head: &'a mut Head,
    attempt: &'a DirectoryAttempt<'d>,
    complete: bool,
}
impl Drop for Opening<'_, '_> {
    fn drop(&mut self) {
        if !self.complete && self.attempt.may_have_acquired() {
            // Native construction/configuration may already have effects.
            self.head.uncertain = true;
        }
    }
}

fn close_result(cleanup: Result<()>, release: ReleaseStatus) -> Result<()> {
    if release == ReleaseStatus::Acknowledged {
        return cleanup;
    }
    let cleanup = match cleanup {
        Ok(()) => CleanupStatus::Succeeded,
        Err(Error::Poisoned) => CleanupStatus::Poisoned,
        Err(_) => CleanupStatus::NativeFailure,
    };
    Err(Error::Close { cleanup, release })
}

fn open_failure(attempt: &DirectoryAttempt<'_>, error: Error) -> Error {
    if attempt.may_have_acquired() {
        close_result(Err(error), attempt.status()).unwrap_err()
    } else {
        error
    }
}

fn gate<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex.try_lock().map_err(|e| match e {
        TryLockError::WouldBlock => Error::Busy,
        TryLockError::Poisoned(_) => Error::Poisoned,
    })
}
fn live(state: &State) -> Result<()> {
    if state.poisoned {
        Err(Error::Poisoned)
    } else {
        Ok(())
    }
}

impl<'d> Engine<'d> {
    /// Open an empty directory, or reopen with the exact caller-held checkpoint.
    /// Existing native contents are verified before opening a mutable writer.
    pub fn open<D: Directory>(
        directory: &'d EngineDirectory<D>,
        schema: Schema,
        checkpoint: Option<Checkpoint>,
    ) -> Result<Self> {
        Self::open_configured(directory, schema, checkpoint, |writer| {
            writer.set_analyzer(Some(Arc::new(analyzer())));
            writer
                .set_ram_buffer_size_mb(8.0)
                .map_err(|_| Error::Native)?;
            writer.set_max_buffered_docs(8).map_err(|_| Error::Native)
        })
    }

    fn open_configured<D: Directory>(
        directory: &'d EngineDirectory<D>,
        schema: Schema,
        checkpoint: Option<Checkpoint>,
        configure: impl FnOnce(&mut IndexWriter<'d>) -> Result<()>,
    ) -> Result<Self> {
        let attempt = directory.begin()?;
        let directory: &'d dyn Directory = &directory.observed;
        let lineage = checkpoint.as_ref().map_or_else(
            || Arc::new(Lineage::default()),
            |checkpoint| Arc::clone(&checkpoint.lineage),
        );
        // Hold the shared lineage through validation and native lock acquisition.
        let mut head = gate(&lineage.head)?;
        if head.uncertain {
            return Err(Error::Checkpoint);
        }
        if let Some(checkpoint) = &checkpoint
            && (checkpoint.schema != schema
                || checkpoint.epoch != head.epoch
                || head.native != Some(checkpoint.native))
        {
            return Err(Error::Checkpoint);
        }
        if head.writer_active {
            return Err(Error::Busy);
        }
        let files = directory.list_all().map_err(|_| Error::Native)?;
        let state = match checkpoint {
            None => {
                if !files.is_empty() {
                    return Err(Error::Checkpoint);
                }
                State::default()
            }
            Some(checkpoint) => {
                if files.is_empty() {
                    return Err(Error::Checkpoint);
                }
                let reader = DirectoryReader::open(directory).map_err(|_| Error::Native)?;
                if NativeCommit::from_infos(&reader.segment_infos) != checkpoint.native {
                    return Err(Error::Checkpoint);
                }
                crate::reader::verify(&reader, &schema, &checkpoint.state.current)?;
                let mut state = checkpoint.state;
                state.physical_documents =
                    usize::try_from(reader.max_doc()).map_err(|_| Error::Limit)?;
                state
            }
        };
        // Declared before writer so failures and unwinding destroy the native
        // owner before the guard invalidates the still-locked lineage head.
        let mut opening = Opening {
            head: &mut head,
            attempt: &attempt,
            complete: false,
        };
        let mut writer = match IndexWriter::open(directory, vec![], "Lucene104", VERSION) {
            Ok(writer) => writer,
            Err(_) => return Err(open_failure(&attempt, Error::Native)),
        };
        writer.set_commit_on_close(false);
        if let Err(error) = configure(&mut writer) {
            drop(writer);
            return Err(open_failure(&attempt, error));
        }
        if !attempt.acquired() {
            drop(writer);
            return Err(Error::Close {
                cleanup: CleanupStatus::Succeeded,
                release: ReleaseStatus::Unobserved,
            });
        }
        opening.head.writer_active = true;
        opening.complete = true;
        drop(opening);
        drop(head);
        Ok(Self {
            writer: Mutex::new(writer),
            directory,
            snapshots: Arc::clone(&lineage.readers),
            lease: WriterLease::new(lineage),
            attempt,
            schema,
            state: Mutex::new(state),
        })
    }

    /// Apply a caller-accepted preparation. Refusal is represented by dropping
    /// preparation; this method does not decide distributed authority.
    /// Native failures may be partial and permanently poison continuation.
    pub fn apply(&self, batch: PreparedBatch) -> Result<Vec<Receipt>> {
        let mut state = gate(&self.state)?;
        live(&state)?;
        if batch.schema != self.schema {
            return Err(Error::Schema);
        }
        if let Some(old) = state.history.iter().find(|h| h.request == batch.request) {
            return if old.operations == batch.operations {
                Ok(old.receipts.clone())
            } else {
                Err(Error::Conflict)
            };
        }
        let additions = batch.documents.iter().filter(|d| d.is_some()).count();
        let new_ids = batch
            .operations
            .iter()
            .filter(|op| !state.current.contains_key(&op.id))
            .count();
        if state.operations + batch.operations.len() > MAX_OPERATIONS
            || state.physical_documents + additions > MAX_DOCUMENTS
            || state.current.len() + new_ids > MAX_DOCUMENTS
            || state.history_bytes + batch.bytes > MAX_HISTORY_BYTES
        {
            return Err(Error::Limit);
        }
        for op in &batch.operations {
            if op.create_only
                && state
                    .current
                    .get(&op.id)
                    .is_some_and(|old| old.fields.is_some())
            {
                return Err(Error::Conflict);
            }
            if state
                .current
                .get(&op.id)
                .is_some_and(|old| old.version >= op.version)
            {
                return Err(Error::Conflict);
            }
        }
        let receipts: Vec<_> = batch
            .operations
            .iter()
            .map(|op| Receipt {
                id: op.id.clone(),
                version: op.version,
                outcome: match (
                    op.fields.is_some(),
                    state
                        .current
                        .get(&op.id)
                        .is_some_and(|old| old.fields.is_some()),
                ) {
                    (true, false) => Outcome::Created,
                    (true, true) => Outcome::Updated,
                    (false, true) => Outcome::Deleted,
                    (false, false) => Outcome::NotFound,
                },
                removed: op.fields.is_none()
                    && state
                        .current
                        .get(&op.id)
                        .is_some_and(|old| old.fields.is_some()),
            })
            .collect();
        let mut writer = gate(&self.writer)?;
        if writer.segment_count() + additions / 8 + 2 > 32 {
            return Err(Error::Limit);
        }
        for (op, document) in batch.operations.iter().zip(batch.documents) {
            let result = if let Some(document) = document {
                writer
                    .update_fields_documents(Term::new("_hs_id", op.id.as_bytes()), &[document])
                    .map(|_| ())
            } else {
                writer
                    .delete_documents_by_term(&[Term::new("_hs_id", op.id.as_bytes())])
                    .map(|_| ())
            };
            if result.is_err() {
                state.poisoned = true;
                self.lease.invalidate();
                return Err(Error::Native);
            }
        }
        state.physical_documents += additions;
        state.operations += batch.operations.len();
        state.history_bytes += batch.bytes;
        for op in &batch.operations {
            state.current.insert(op.id.clone(), op.clone());
        }
        state.history.push(History {
            request: batch.request,
            operations: batch.operations,
            receipts: receipts.clone(),
        });
        Ok(receipts)
    }

    /// Publish a frozen native NRT reader under the same admission cut as mutation.
    pub fn refresh(&self) -> Result<crate::reader::Snapshot> {
        let mut state = gate(&self.state)?;
        live(&state)?;
        let reservation = crate::reader::Reservation::new(&self.snapshots)?;
        let reader = match DirectoryReader::open_from_writer(&self.writer) {
            Ok(r) => r,
            Err(_) => {
                state.poisoned = true;
                self.lease.invalidate();
                return Err(Error::Native);
            }
        };
        crate::reader::Snapshot::new(reader, self.schema.clone(), reservation)
    }

    /// Commit native files and return caller-held retry state. This is not a
    /// distributed durability receipt or an authoritative hsearch checkpoint.
    pub fn commit(&self) -> Result<Checkpoint> {
        let mut state = gate(&self.state)?;
        live(&state)?;
        let mut head = gate(&self.lease.lineage.head)?;
        if head.uncertain {
            return Err(Error::Poisoned);
        }
        let epoch = head.epoch.checked_add(1).ok_or(Error::Limit)?;
        if gate(&self.writer)?.commit().is_err() {
            state.poisoned = true;
            head.uncertain = true;
            return Err(Error::Native);
        }
        self.complete_checkpoint(&mut state, &mut head, epoch)
    }

    // Kept separate to exercise the real post-native-commit failure boundary:
    // the native commit can succeed before reading its identity fails.
    fn complete_checkpoint(
        &self,
        state: &mut State,
        head: &mut Head,
        epoch: u64,
    ) -> Result<Checkpoint> {
        let native = match NativeCommit::read(self.directory) {
            Ok(native) => native,
            Err(error) => {
                state.poisoned = true;
                head.uncertain = true;
                return Err(error);
            }
        };
        // Advance even if native commit identity stayed unchanged: request and
        // tombstone history can advance without changing any live document.
        head.epoch = epoch;
        head.native = Some(native);
        Ok(Checkpoint {
            schema: self.schema.clone(),
            state: state.clone(),
            lineage: Arc::clone(&self.lease.lineage),
            epoch,
            native,
        })
    }

    /// Explicit bounded maintenance. Merge is never performed by search.
    pub fn merge(&self) -> Result<()> {
        let mut state = gate(&self.state)?;
        live(&state)?;
        if gate(&self.writer)?.force_merge(1).is_err() {
            state.poisoned = true;
            self.lease.invalidate();
            return Err(Error::Native);
        }
        state.physical_documents = state
            .current
            .values()
            .filter(|op| op.fields.is_some())
            .count();
        Ok(())
    }

    /// Close without committing pending work. Previously returned snapshots
    /// remain usable. Close consumes the adapter, including on native failure.
    pub fn close(self) -> Result<()> {
        let Self {
            writer,
            lease,
            attempt,
            ..
        } = self;
        let mut closing = Closing { lease, attempt };
        let cleanup = match writer.into_inner() {
            Ok(writer) => writer.close().map_err(|_| Error::Native),
            Err(poisoned) => {
                // Consume the poisoned owner before inspecting release too.
                drop(poisoned.into_inner());
                Err(Error::Poisoned)
            }
        };
        let result = close_result(cleanup, closing.attempt.status());
        let finished = closing.lease.finish(result.is_ok());
        // Admission remains occupied until both native teardown and lineage
        // completion. Implicit drop never reaches finish(true).
        drop(closing);
        result.and(finished)
    }
}

#[cfg(test)]
#[path = "adapter_tests.rs"]
mod tests;
