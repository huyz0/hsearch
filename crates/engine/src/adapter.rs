//! Serialized admission around a caller-owned directory and native writer.
use std::collections::{BTreeMap, VecDeque};
use std::sync::{
    Arc, Mutex, MutexGuard, RwLock, TryLockError,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use lucene_index::buffered_updates::Term;
use lucene_index::concurrent_writer::ConcurrentIndexWriter;
use lucene_index::document::Document;
use lucene_index::index_writer::IndexWriter;
use lucene_index::merge_policy::MergePolicyConfig;
use lucene_index::segment_info::LuceneVersion;
use lucene_search::directory_reader::DirectoryReader;

use crate::checkpoint::{Head, Lineage, NativeCommit, WriterLease};
use crate::directory::DirectoryAttempt;
use crate::schema::{MAX_DOCUMENT_BYTES, analyzer};
use crate::{
    CleanupStatus, Directory, EngineDirectory, Error, Kind, ReleaseStatus, Result, Schema, Value,
};

const VERSION: LuceneVersion = LuceneVersion {
    major: 10,
    minor: 5,
    bugfix: 0,
};
const MAX_BATCH: usize = 16;
const MAX_BATCH_BYTES: usize = 65_536;

/// Most bytes one prepared batch carries: the research profile's bound, plus
/// whatever the schema's document limit allows beyond the research document, so
/// one maximal document always fits a batch alone.
fn batch_bytes(schema: &Schema) -> usize {
    MAX_BATCH_BYTES.saturating_add(
        schema
            .limits
            .document_bytes
            .saturating_sub(MAX_DOCUMENT_BYTES),
    )
}

/// Caller-resolved mutation. The adapter never generates IDs or versions.
#[derive(Clone, Copy, Debug, PartialEq)]
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

/// One admitted mutation as the engine remembers it for retry recognition: the
/// identity, version and kind, and a digest of the field values, never the
/// values. An exact retry (same request, same bytes) has the same digest, so a
/// large document is not held a second time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Operation {
    id: String,
    create_only: bool,
    pub(crate) version: i64,
    /// `None` for a delete.
    pub(crate) digest: Option<u128>,
    /// Native documents it adds: the root and each nested object. 0 for a delete.
    docs: usize,
}

/// Nested objects a value carries.
fn objects_in(value: &Value<'_>) -> usize {
    match value {
        Value::Nested(objects) => objects.len(),
        _ => 0,
    }
}

/// The stored forms of one value, in the order the digest reads them. A nested value is its
/// object count, then each object's fields in declared order; a join value is its relation
/// and the parent id it is stored under, which for a parent is its own id.
pub(crate) fn stored_of<'a>(id: &'a str, value: &'a Value<'a>, out: &mut Vec<Stored<'a>>) {
    match value {
        Value::String(s) => out.push(Stored::Text(s)),
        Value::Shared(s) => out.push(Stored::Text(s)),
        Value::Boolean(b) => out.push(Stored::Text(if *b { "true" } else { "false" })),
        Value::Integer(n) | Value::Date(n) => out.push(Stored::Long(*n)),
        Value::Vector(v) => out.push(Stored::Floats(v)),
        Value::Nested(objects) => {
            out.push(Stored::Long(objects.len() as i64));
            for object in *objects {
                for (_, v) in *object {
                    stored_of(id, v, out);
                }
            }
        }
        Value::Join { relation, parent } => {
            out.push(Stored::Text(relation));
            out.push(Stored::Text(parent.unwrap_or(id)));
        }
    }
}

impl Operation {
    fn new(mutation: &Mutation<'_>, schema: &Schema) -> Self {
        match mutation {
            Mutation::Put {
                id,
                version,
                fields,
            }
            | Mutation::Create {
                id,
                version,
                fields,
            } => Self {
                id: id.to_string(),
                create_only: matches!(mutation, Mutation::Create { .. }),
                version: *version,
                digest: Some(digest({
                    let mut stored = Vec::with_capacity(fields.len() * 2);
                    for (at, (name, value)) in fields.iter().enumerate() {
                        // The field's schema position first: a document without an
                        // added field must not digest like one that lacks another.
                        let place = match schema.fields.get(at) {
                            Some((declared, _)) if declared == name => at,
                            _ => schema
                                .fields
                                .iter()
                                .position(|(n, _)| n == name)
                                .unwrap_or(at),
                        };
                        stored.push(Stored::Long(place as i64));
                        stored_of(id, value, &mut stored);
                    }
                    stored.into_iter()
                })),
                docs: 1 + fields.iter().map(|(_, v)| objects_in(v)).sum::<usize>(),
            },
            Mutation::Delete { id, version } => Self {
                id: id.to_string(),
                create_only: false,
                version: *version,
                digest: None,
                docs: 0,
            },
        }
    }

    fn live(&self) -> Live {
        Live {
            version: self.version,
            digest: self.digest,
            docs: self.docs,
        }
    }
}

/// What [`Engine::admit`] found.
#[derive(Debug, Eq, PartialEq)]
pub enum Checked {
    /// The batch has not been applied and would be accepted now.
    Fresh,
    /// The same request with the same operations was applied before: these were its receipts.
    Recognised(Vec<Receipt>),
}

/// Validated private preparation. Dropping it has no writer effects.
pub struct PreparedBatch {
    schema: Schema,
    request: String,
    operations: Vec<Operation>,
    documents: Vec<Option<Vec<Document>>>,
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
                self.check_row(fields)?;
                for (at, (name, value)) in fields.iter().enumerate() {
                    // The declared field at the same position, unless an added field
                    // was left out before this one.
                    let kind = match self.fields.get(at) {
                        Some((declared, kind)) if declared == name => *kind,
                        _ => self.kind(name)?,
                    };
                    crate::schema::validate(kind, *value)?;
                    bytes = bytes
                        .checked_add(name.len())
                        .and_then(|n| n.checked_add(crate::schema::value_bytes(value)))
                        .ok_or(Error::Limit)?;
                    if bytes > batch_bytes(self) {
                        return Err(Error::Limit);
                    }
                }
            }
            if bytes > batch_bytes(self) {
                return Err(Error::Limit);
            }
        }
        let operations: Vec<_> = mutations.iter().map(|m| Operation::new(m, self)).collect();
        let documents = mutations
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
                } => self.block(id, *version, fields).map(Some),
                Mutation::Delete { .. } => Ok(None),
            })
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

/// What the engine keeps of each live identity: its version and a digest of its
/// field values, never the values. Checkpoint verification compares a reopened
/// index against the digest; holding the text would pin every document ever
/// indexed in memory, which a large-document limit makes untenable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Live {
    pub(crate) version: i64,
    /// `None` for a tombstone.
    pub(crate) digest: Option<u128>,
    /// Native documents it holds: the root and each nested object.
    pub(crate) docs: usize,
}

/// Digest of typed field values in schema order, over their stored forms (a
/// boolean is stored as its text, an integer or date as a long). Two SipHash-1-3
/// passes under two independent random keys drawn once per process, so nobody who
/// can send a document can build a colliding one offline. A digest only has to
/// agree with itself inside one process (a checkpoint is in-process, never
/// persisted), so the keys need not survive a restart.
pub(crate) fn digest<'a>(values: impl Iterator<Item = Stored<'a>>) -> u128 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::OnceLock;
    static KEYS: OnceLock<(RandomState, RandomState)> = OnceLock::new();
    let (first, second) = KEYS.get_or_init(|| (RandomState::new(), RandomState::new()));
    let (mut a, mut b) = (first.build_hasher(), second.build_hasher());
    for value in values {
        match value {
            Stored::Text(s) => {
                for h in [&mut a, &mut b] {
                    h.write_u8(1);
                    h.write_u64(s.len() as u64);
                    h.write(s.as_bytes());
                }
            }
            Stored::Long(n) => {
                for h in [&mut a, &mut b] {
                    h.write_u8(2);
                    h.write_i64(n);
                }
            }
            Stored::Floats(v) => {
                for h in [&mut a, &mut b] {
                    h.write_u8(3);
                    h.write_u64(v.len() as u64);
                    for c in v {
                        h.write_u32(c.to_bits());
                    }
                }
            }
        }
    }
    (u128::from(a.finish()) << 64) | u128::from(b.finish())
}

/// A field value in the form the index stores it.
pub(crate) enum Stored<'a> {
    Text(&'a str),
    Long(i64),
    /// A vector's components, compared bit for bit.
    Floats(&'a [f32]),
}

#[derive(Clone)]
struct History {
    bytes: usize,
    request: String,
    operations: Vec<Operation>,
    receipts: Vec<Receipt>,
}
#[derive(Clone, Default)]
struct State {
    current: BTreeMap<String, Live>,
    history: VecDeque<History>,
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

impl Checkpoint {
    /// The versions of the identities deleted as of this checkpoint's commit, for writing beside
    /// a base so [`Engine::open_recovered`] can refuse a stale put over a deleted identity.
    pub fn tombstones(&self) -> Result<Vec<u8>> {
        crate::tombstones::encode(&self.state.current, self.native.to_bytes())
    }
}

/// Where an engine's automatic merges run. A cluster default, chosen per index
/// when its engine is opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MergeMode {
    /// Inside the refresh that triggers them, under the writer lock.
    #[default]
    Inline,
    /// On a thread calling [`Engine::run_background_merges`]: a refresh never
    /// waits for a merge. Needs `auto_merge_mb > 0`.
    Background,
}

/// The native writer: the single-threaded one behind its lock, or the
/// concurrent one whose merges a background thread runs.
enum Writer<'d> {
    Inline(Box<Mutex<IndexWriter<'d>>>),
    /// The flag is set when a native call panicked, as a poisoned lock would.
    Background(Box<ConcurrentIndexWriter<'d>>, AtomicBool),
}

#[cfg(test)]
impl<'d> Writer<'d> {
    fn lock(&self) -> std::sync::LockResult<MutexGuard<'_, IndexWriter<'d>>> {
        match self {
            Writer::Inline(m) => m.lock(),
            Writer::Background(..) => panic!("background writer has no lock"),
        }
    }
}

/// The writer as held for one admitted call.
enum Held<'a, 'd> {
    Inline(MutexGuard<'a, IndexWriter<'d>>),
    Background(&'a ConcurrentIndexWriter<'d>, &'a AtomicBool),
}

impl Held<'_, '_> {
    fn segment_count(&mut self) -> usize {
        match self {
            Held::Inline(w) => w.segment_count(),
            Held::Background(w, panicked) => {
                guarded(panicked, || w.with_writer(|w| w.segment_count()))
            }
        }
    }

    fn update(&mut self, term: Term, block: &[Document]) -> bool {
        match self {
            Held::Inline(w) => w.update_fields_documents(term, block).is_ok(),
            Held::Background(w, panicked) => {
                guarded(panicked, || w.update_fields_documents(term, block).is_ok())
            }
        }
    }

    fn delete(&mut self, term: Term) -> bool {
        match self {
            Held::Inline(w) => w.delete_documents_by_term(&[term]).is_ok(),
            Held::Background(w, panicked) => {
                guarded(panicked, || w.delete_documents_by_term(&[term]).is_ok())
            }
        }
    }

    fn commit(&mut self) -> bool {
        match self {
            Held::Inline(w) => w.commit().is_ok(),
            Held::Background(w, panicked) => guarded(panicked, || w.commit().is_ok()),
        }
    }

    fn force_merge(&mut self) -> bool {
        match self {
            Held::Inline(w) => w.force_merge(1).is_ok(),
            Held::Background(w, panicked) => guarded(panicked, || {
                w.with_writer_merges_aborted(|w| w.force_merge(1)).is_ok()
            }),
        }
    }
}

/// Runs a native call, recording a panic in `panicked` before it continues.
fn guarded<T>(panicked: &AtomicBool, call: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(value) => value,
        Err(payload) => {
            panicked.store(true, Ordering::Release);
            std::panic::resume_unwind(payload)
        }
    }
}

impl<'d> Writer<'d> {
    fn hold(&self) -> Result<Held<'_, 'd>> {
        match self {
            Writer::Inline(m) => Ok(Held::Inline(gate(m)?)),
            Writer::Background(w, panicked) => {
                if panicked.load(Ordering::Acquire) {
                    Err(Error::Poisoned)
                } else {
                    Ok(Held::Background(w, panicked))
                }
            }
        }
    }
}

/// Caller-owned-directory adapter with one writer and bounded nonblocking admission.
/// Only methods on this adapter may mutate its directory while it is open.
pub struct Engine<'d> {
    // Declaration order is part of WriterLease's implicit-drop invariant.
    writer: Writer<'d>,
    lease: WriterLease,
    attempt: DirectoryAttempt<'d>,
    directory: &'d dyn Directory,
    snapshots: Arc<AtomicUsize>,
    /// Replaced only by [`Engine::evolve_schema`], with one that extends it.
    schema: RwLock<Schema>,
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

/// The engine's current schema. Held only to clone or compare, never across I/O.
fn current(schema: &RwLock<Schema>) -> Result<Schema> {
    schema
        .read()
        .map(|s| s.clone())
        .map_err(|_| Error::Poisoned)
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
        Self::open_merging(directory, schema, checkpoint, MergeMode::Inline)
    }

    /// [`Engine::open`] with the automatic merges run as `mode` says.
    pub fn open_merging<D: Directory>(
        directory: &'d EngineDirectory<D>,
        schema: Schema,
        checkpoint: Option<Checkpoint>,
        mode: MergeMode,
    ) -> Result<Self> {
        Self::open_with(directory, schema, checkpoint, None, mode)
    }

    fn open_with<D: Directory>(
        directory: &'d EngineDirectory<D>,
        schema: Schema,
        checkpoint: Option<Checkpoint>,
        recovered: Option<([u8; 32], BTreeMap<String, i64>)>,
        mode: MergeMode,
    ) -> Result<Self> {
        let background = mode == MergeMode::Background && schema.limits.auto_merge_mb > 0;
        let buffered = schema.limits.buffered_documents;
        let auto_merge_mb = schema.limits.auto_merge_mb;
        let vectors: Vec<(String, crate::VectorSpec)> = schema
            .fields
            .iter()
            .filter_map(|(name, kind)| match kind {
                Kind::Vector(spec) => Some((name.clone(), *spec)),
                _ => None,
            })
            .collect();
        // Room for a full buffer of worst-case documents, never below 8 MiB.
        let ram_mb = (buffered as f64 * MAX_DOCUMENT_BYTES as f64 / 1_048_576.0).clamp(8.0, 256.0);
        Self::open_configured_as(
            directory,
            schema,
            checkpoint,
            recovered,
            background,
            move |writer| {
                writer.set_analyzer(Some(Arc::new(analyzer())));
                if background {
                    // No merge on a refresh's full flush: the merge thread owns merging.
                    writer.set_max_full_flush_merge_wait_millis(0);
                }
                for (name, spec) in &vectors {
                    writer.set_knn_vectors_format_for_field(name, spec.format());
                }
                writer
                    .set_ram_buffer_size_mb(ram_mb)
                    .map_err(|_| Error::Native)?;
                if auto_merge_mb > 0 {
                    writer.set_merge_policy(Some(MergePolicyConfig {
                        max_merged_segment_size: (auto_merge_mb as u64) << 20,
                        ..MergePolicyConfig::default()
                    }));
                }
                writer
                    .set_max_buffered_docs(buffered as i32)
                    .map_err(|_| Error::Native)
            },
        )
    }

    #[cfg(test)]
    fn open_configured<D: Directory>(
        directory: &'d EngineDirectory<D>,
        schema: Schema,
        checkpoint: Option<Checkpoint>,
        configure: impl FnOnce(&mut IndexWriter<'d>) -> Result<()>,
    ) -> Result<Self> {
        Self::open_configured_as(directory, schema, checkpoint, None, false, configure)
    }

    fn open_configured_as<D: Directory>(
        directory: &'d EngineDirectory<D>,
        schema: Schema,
        checkpoint: Option<Checkpoint>,
        recovered: Option<([u8; 32], BTreeMap<String, i64>)>,
        background: bool,
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
            && (!schema.extends(&checkpoint.schema)
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
            None if recovered.is_some() => {
                // Opened on the files of a base alone: the index says what is live, the
                // tombstone list says what was deleted, and nothing else is remembered.
                if files.is_empty() {
                    return Err(Error::Checkpoint);
                }
                let reader = DirectoryReader::open(directory).map_err(|_| Error::Native)?;
                let native = NativeCommit::from_infos(&reader.segment_infos);
                let (taken_at, tombstones) = recovered.ok_or(Error::Checkpoint)?;
                // The list must describe these files: another commit's list would silently
                // forget a delete or invent one.
                if taken_at != native.to_bytes() {
                    return Err(Error::Checkpoint);
                }
                let mut state = State {
                    current: crate::reader::rebuild(&reader, &schema)?,
                    ..State::default()
                };
                for (id, version) in tombstones {
                    if state
                        .current
                        .insert(
                            id,
                            Live {
                                version,
                                digest: None,
                                docs: 0,
                            },
                        )
                        .is_some()
                    {
                        // A deleted identity that is also live is not a state this index was in.
                        return Err(Error::Checkpoint);
                    }
                }
                state.physical_documents =
                    usize::try_from(reader.max_doc()).map_err(|_| Error::Limit)?;
                head.native = Some(native);
                state
            }
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
        let writer = if background {
            // Merges run on the thread that calls `run_background_merges`;
            // the writer registers none of its own.
            match ConcurrentIndexWriter::new(writer, 1) {
                Ok(w) => Writer::Background(Box::new(w), AtomicBool::new(false)),
                Err(_) => return Err(open_failure(&attempt, Error::Native)),
            }
        } else {
            Writer::Inline(Box::new(Mutex::new(writer)))
        };
        Ok(Self {
            writer,
            directory,
            snapshots: Arc::clone(&lineage.readers),
            lease: WriterLease::new(lineage),
            attempt,
            schema: RwLock::new(schema),
            state: Mutex::new(state),
        })
    }

    /// Open on the files of a committed base and the tombstone list written with it
    /// ([`Checkpoint::tombstones`]), in a process that holds no checkpoint. Live documents,
    /// their versions and digests are read back from the index; deleted identities keep their
    /// versions from the list. Request history is not restored: a retry of a request from before
    /// the base is not recognised, though its version is still refused as stale. Anything that does
    /// not match is `Error::Checkpoint`.
    pub fn open_recovered<D: Directory>(
        directory: &'d EngineDirectory<D>,
        schema: Schema,
        tombstones: &[u8],
        mode: MergeMode,
    ) -> Result<Self> {
        let (taken_at, recovered) = crate::tombstones::decode(tombstones)?;
        Self::open_with(directory, schema, None, Some((taken_at, recovered)), mode)
    }

    /// Every refusal [`Engine::apply`] can make before it changes anything, decided against the
    /// state held in `state`, without changing it.
    fn precheck(&self, state: &State, batch: &PreparedBatch) -> Result<Checked> {
        let limits = {
            let held = self.schema.read().map_err(|_| Error::Poisoned)?;
            if batch.schema != *held {
                return Err(Error::Schema);
            }
            held.limits
        };
        if let Some(old) = state.history.iter().find(|h| h.request == batch.request) {
            return if old.operations == batch.operations {
                Ok(Checked::Recognised(old.receipts.clone()))
            } else {
                Err(Error::Conflict)
            };
        }
        let additions: usize = batch.documents.iter().flatten().map(Vec::len).sum();
        let new_ids = batch
            .operations
            .iter()
            .filter(|op| !state.current.contains_key(&op.id))
            .count();
        if state.operations + batch.operations.len() > limits.operations
            || state.physical_documents + additions > limits.documents
            || state.current.len() + new_ids > limits.documents
            || (!limits.window && state.history_bytes + batch.bytes > limits.history_bytes)
        {
            return Err(Error::Limit);
        }
        for op in &batch.operations {
            if op.create_only
                && state
                    .current
                    .get(&op.id)
                    .is_some_and(|old| old.digest.is_some())
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
        let additions: usize = batch.documents.iter().flatten().map(Vec::len).sum();
        let mut held = self.writer.hold()?;
        if held.segment_count() + additions / limits.buffered_documents + 2 > limits.segments {
            return Err(Error::Limit);
        }
        Ok(Checked::Fresh)
    }

    /// Whether [`Engine::apply`] would accept `batch` now, changing nothing: a batch already
    /// applied (same request, same operations) is `Recognised` with its receipts. A caller that
    /// serialises its writes can commit a `Fresh` batch to a log first and apply it after, and
    /// the apply is refused only if a native call fails (which poisons the engine) or the
    /// segment bound is crossed by a flush in between.
    pub fn admit(&self, batch: &PreparedBatch) -> Result<Checked> {
        let state = gate(&self.state)?;
        live(&state)?;
        self.precheck(&state, batch)
    }

    /// Apply a caller-accepted preparation. Refusal is represented by dropping
    /// preparation; this method does not decide distributed authority.
    /// Native failures may be partial and permanently poison continuation.
    pub fn apply(&self, batch: PreparedBatch) -> Result<Vec<Receipt>> {
        let mut state = gate(&self.state)?;
        live(&state)?;
        if let Checked::Recognised(receipts) = self.precheck(&state, &batch)? {
            return Ok(receipts);
        }
        let limits = self.schema.read().map_err(|_| Error::Poisoned)?.limits;
        let additions: usize = batch.documents.iter().flatten().map(Vec::len).sum();
        let receipts: Vec<_> = batch
            .operations
            .iter()
            .map(|op| Receipt {
                id: op.id.clone(),
                version: op.version,
                outcome: match (
                    op.digest.is_some(),
                    state
                        .current
                        .get(&op.id)
                        .is_some_and(|old| old.digest.is_some()),
                ) {
                    (true, false) => Outcome::Created,
                    (true, true) => Outcome::Updated,
                    (false, true) => Outcome::Deleted,
                    (false, false) => Outcome::NotFound,
                },
                removed: op.digest.is_none()
                    && state
                        .current
                        .get(&op.id)
                        .is_some_and(|old| old.digest.is_some()),
            })
            .collect();
        let mut writer = self.writer.hold()?;
        for (op, document) in batch.operations.iter().zip(batch.documents) {
            let term = Term::new("_hs_id", op.id.as_bytes());
            let done = if let Some(block) = document {
                writer.update(term, &block)
            } else {
                writer.delete(term)
            };
            if !done {
                state.poisoned = true;
                self.lease.invalidate();
                return Err(Error::Native);
            }
        }
        state.physical_documents += additions;
        state.operations += batch.operations.len();
        state.history_bytes += batch.bytes;
        for op in &batch.operations {
            state.current.insert(op.id.clone(), op.live());
        }
        if limits.window {
            // Retry identity beyond the window belongs to the caller's log.
            while state.history_bytes > limits.history_bytes {
                let Some(old) = state.history.pop_front() else {
                    break;
                };
                state.history_bytes -= old.bytes;
            }
        }
        state.history.push_back(History {
            bytes: batch.bytes,
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
        let reader = match match &self.writer {
            Writer::Inline(m) => DirectoryReader::open_from_writer(&**m),
            Writer::Background(w, panicked) => {
                guarded(panicked, || DirectoryReader::open_from_writer(&**w))
            }
        } {
            Ok(r) => r,
            Err(_) => {
                state.poisoned = true;
                self.lease.invalidate();
                return Err(Error::Native);
            }
        };
        crate::reader::Snapshot::new(reader, current(&self.schema)?, reservation)
    }

    /// Replace the schema with one that extends it ([`Schema::evolve`]): fields added
    /// to a live engine. Serialised with [`Engine::apply`], so every batch sees
    /// one schema or the other, never a mix; a batch prepared under the old one is
    /// refused afterwards with [`Error::Schema`]. Snapshots already open keep the
    /// schema they were taken under. A schema that is not this one extended is refused.
    pub fn evolve_schema(&self, schema: Schema) -> Result<()> {
        let state = gate(&self.state)?;
        live(&state)?;
        let mut held = self.schema.write().map_err(|_| Error::Poisoned)?;
        if *held == schema {
            return Ok(());
        }
        if !schema.extends(&held) || schema.revision <= held.revision {
            return Err(Error::Schema);
        }
        *held = schema;
        Ok(())
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
        if !self.writer.hold()?.commit() {
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
            schema: current(&self.schema)?,
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
        if !self.writer.hold()?.force_merge() {
            state.poisoned = true;
            self.lease.invalidate();
            return Err(Error::Native);
        }
        state.physical_documents = state
            .current
            .values()
            .filter(|live| live.digest.is_some())
            .map(|live| live.docs)
            .sum();
        Ok(())
    }

    /// [`Engine::merge`] after the scheduler admits it: a free slot, and disk
    /// reserved for as many bytes as the directory holds now (a merge writes its
    /// output before it frees its inputs). Refused with `Busy`, `Limit` or
    /// `Unsupported` (shut down) before any native work starts.
    pub fn merge_scheduled(&self, scheduler: &crate::MergeScheduler) -> Result<()> {
        let mut bytes = 0u64;
        for name in self.directory.list_all().map_err(|_| Error::Native)? {
            bytes = bytes.saturating_add(
                self.directory
                    .file_length(&name)
                    .map_err(|_| Error::Native)?,
            );
        }
        let _ticket = scheduler.admit(bytes)?;
        self.merge()
    }

    /// Runs `read` with no merge running or starting, so the directory holds
    /// only whole files: a merge's output is written without any lock the
    /// caller shares, and a failed merge deletes it. Waits for a merge in
    /// progress to finish. An inline engine merges only inside its own calls, so
    /// `read` runs at once.
    pub fn while_merges_are_paused<R>(&self, read: impl FnOnce() -> R) -> Result<R> {
        match &self.writer {
            Writer::Inline(_) => Ok(read()),
            Writer::Background(w, panicked) => {
                if panicked.load(Ordering::Acquire) {
                    return Err(Error::Poisoned);
                }
                Ok(guarded(panicked, || {
                    w.with_writer_merges_aborted(|_| read())
                }))
            }
        }
    }

    /// The merge thread of a [`MergeMode::Background`] engine: runs the policy's
    /// merges as segments appear, at most [`crate::background::MAX_CONCURRENT_MERGES`]
    /// at once across the process, until `stop` is set; returns how many ran.
    /// Returns at once for an inline engine. A failed or panicking merge poisons
    /// the engine, as a failed inline merge does.
    pub fn run_background_merges(&self, stop: &AtomicBool) -> usize {
        let Writer::Background(writer, panicked) = &self.writer else {
            return 0;
        };
        let mut merged = 0;
        let mut idle = std::time::Duration::from_millis(10);
        while !stop.load(Ordering::Acquire) {
            let Some(slot) = crate::background::SLOTS.acquire(std::time::Duration::from_millis(50))
            else {
                continue;
            };
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| writer.maybe_merge()));
            drop(slot);
            match outcome {
                Ok(Ok(n)) => {
                    merged += n;
                    idle = if n > 0 {
                        std::time::Duration::from_millis(10)
                    } else {
                        (idle * 2).min(std::time::Duration::from_millis(250))
                    };
                }
                failed => {
                    if failed.is_err() {
                        panicked.store(true, Ordering::Release);
                    }
                    let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.poisoned = true;
                    drop(state);
                    self.lease.invalidate();
                    return merged;
                }
            }
            std::thread::sleep(idle);
        }
        merged
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
        let cleanup = match writer {
            Writer::Inline(m) => match m.into_inner() {
                Ok(writer) => writer.close().map_err(|_| Error::Native),
                Err(poisoned) => {
                    // Consume the poisoned owner before inspecting release too.
                    drop(poisoned.into_inner());
                    Err(Error::Poisoned)
                }
            },
            Writer::Background(w, panicked) => {
                if panicked.load(Ordering::Acquire) {
                    drop(w);
                    Err(Error::Poisoned)
                } else {
                    w.stop_merges();
                    w.into_writer()
                        .and_then(IndexWriter::close)
                        .map_err(|_| Error::Native)
                }
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

#[cfg(test)]
#[path = "background_merge_tests.rs"]
mod background_merge_tests;
