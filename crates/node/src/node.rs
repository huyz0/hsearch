//! The node's operations over indexes held in memory and rebuilt from the
//! object store: create, bulk, get, search and refresh.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use hsearch_engine::{Kind, Outcome, StoredValue};
use hsearch_format::{Digest, Layout};
use hsearch_index::{Field, Op, Wait};
use hsearch_object::{LargeObjectWriter, MetadataStore};
use hsearch_store::served::{ServedIndex, Settings};
use serde_json::{Map, Value, json};
use typed_arena::Arena;

use crate::ApiError;
use crate::bulk::{self, Action, Item};
use crate::limits::{Limits, MemoryGate};
use crate::mapping::{Mapping, infer, typed_owned, valid_field_name};
use crate::policy::{Dynamic, MappingLimits};
use crate::search::{self, Source};

mod get;

/// Operations per call to the write path, below its slot bound.
const CHUNK: usize = 1000;
/// A document of at least this many bytes is charged its indexing working set
/// against the node's memory budget; smaller ones ride on the request's own
/// reservation, as before.
const LARGE_DOCUMENT_BYTES: usize = 64 * 1024;
/// Bytes charged per byte of a large document while its batch is written. Measured
/// (`large_field_profile`, in-memory store, so it includes the store's retained
/// copies): peak RSS was 29x a 0.25 MiB document, 16x a 1 MiB one and 12.6x a 3.9 MiB
/// one, so 32 covers the whole range a charge applies to.
const INDEX_COST_FACTOR: u64 = 32;

struct Entry {
    index: ServedIndex,
    /// The mapping version writers and searches use now: one parsed copy per node,
    /// shared by every index with this mapping. Replaced only by a mapping change,
    /// after the index itself has evolved to it.
    mapping: RwLock<Arc<Mapping>>,
    /// The reference stored for the mapping, as last read or written. Held for the
    /// whole of a mapping change, so changes to one index run one at a time and
    /// each replaces exactly what the one before it left.
    stored: Mutex<Vec<u8>>,
}

impl Entry {
    fn new(index: ServedIndex, mapping: Arc<Mapping>, stored: Vec<u8>) -> Self {
        Self {
            index,
            mapping: RwLock::new(mapping),
            stored: Mutex::new(stored),
        }
    }

    /// The mapping version to build one request's documents or query under.
    fn mapping(&self) -> Arc<Mapping> {
        Arc::clone(&self.mapping.read().unwrap_or_else(|p| p.into_inner()))
    }
}

/// The deployment name this single-process node keys shared objects under.
const DEPLOYMENT: &str = "local";
/// Parsed shared mappings kept per node. Beyond this a mapping is still
/// served, parsed per index, so the cache cannot grow with the index count.
const MAX_SHARED_MAPPINGS: usize = 4096;

/// One node: a store, a scratch directory and the indexes opened so far.
pub struct Node<S> {
    store: Arc<S>,
    data_dir: PathBuf,
    indexes: Mutex<HashMap<String, Slot>>,
    limits: Limits,
    mapping_limits: MappingLimits,
    background_merge: bool,
    gate: MemoryGate,
    /// Shared mappings this node has stored or verified, by content hash.
    shared: Mutex<HashMap<Digest, Arc<Mapping>>>,
}

/// One index's place in the node. The map lock is held only to find or add a
/// slot; the slot's own lock is held across the store I/O of opening it, so a
/// cold index never blocks another.
type Slot = Arc<Mutex<Option<Arc<Entry>>>>;

fn poisoned() -> ApiError {
    ApiError::new(500, "internal_exception", "node state poisoned")
}

/// A writer that stopped for good and must be reopened from the store.
fn stopped(error: &hsearch_index::Error) -> bool {
    use hsearch_index::Error::{AppliedFailed, Closed, Fenced, Poisoned};
    matches!(error, Fenced | Poisoned | Closed | AppliedFailed)
}

/// The write path's identity bound, checked per item so one bad id fails alone.
fn check_id(id: &str) -> Result<(), ApiError> {
    if id.is_empty() || id.len() > 256 {
        return Err(ApiError::bad("_id must be 1-256 bytes"));
    }
    Ok(())
}

/// Index names follow the write path's rule, checked here for a clear error.
pub(crate) fn valid_index(name: &str) -> Result<(), ApiError> {
    if name.is_empty()
        || name == "v1"
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(ApiError::new(
            400,
            "invalid_index_name_exception",
            "index names are 1-64 of [a-z0-9_-]; v1 is reserved",
        ));
    }
    Ok(())
}

fn json_value(value: &StoredValue) -> Value {
    match value {
        StoredValue::String(s) => json!(s),
        StoredValue::Integer(n) | StoredValue::Date(n) => json!(n),
        StoredValue::Boolean(b) => json!(b),
    }
}

impl<S: MetadataStore + LargeObjectWriter + 'static> Node<S> {
    /// A node over `store`, keeping scratch engines under `data_dir`, with the
    /// default request limits.
    pub fn new(store: Arc<S>, data_dir: PathBuf) -> Self {
        Self::with_limits(store, data_dir, Limits::default()).expect("the default limits are valid")
    }

    /// A node with explicit request limits, refused if they cannot work.
    pub fn with_limits(store: Arc<S>, data_dir: PathBuf, limits: Limits) -> Result<Self, String> {
        limits.validate()?;
        Ok(Self {
            store,
            data_dir,
            indexes: Mutex::new(HashMap::new()),
            gate: MemoryGate::new(limits.memory_budget_bytes),
            shared: Mutex::new(HashMap::new()),
            limits,
            mapping_limits: MappingLimits::default(),
            background_merge: false,
        })
    }

    /// This node running every index's automatic merges on a background thread
    /// instead of inside the refresh that triggers them (needs `auto_merge_mb`).
    #[must_use]
    pub fn with_background_merge(mut self, on: bool) -> Self {
        self.background_merge = on;
        self
    }

    /// How indexes are served under this node's settings.
    fn settings(&self) -> Settings {
        let mut settings =
            Settings::new(Layout::new(DEPLOYMENT).expect("the deployment name is valid"));
        if self.background_merge {
            settings.merge = hsearch_engine::MergeMode::Background;
        }
        settings
    }

    /// This node under explicit field, depth and nested-object limits (PR-42),
    /// refused if they cannot work.
    pub fn with_mapping_limits(mut self, limits: MappingLimits) -> Result<Self, String> {
        limits.validate()?;
        self.mapping_limits = limits;
        Ok(self)
    }

    /// The field, depth and nested-object limits in force.
    #[must_use]
    pub fn mapping_limits(&self) -> &MappingLimits {
        &self.mapping_limits
    }

    /// How many distinct parsed mappings this node holds: one per distinct
    /// mapping in use, however many indexes share it.
    #[must_use]
    pub fn shared_mapping_count(&self) -> usize {
        self.shared.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// The request limits in force.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn scratch(&self, name: &str) -> PathBuf {
        self.data_dir.join(name)
    }

    fn slot(&self, name: &str) -> Result<Slot, ApiError> {
        let mut map = self.indexes.lock().map_err(|_| poisoned())?;
        Ok(Arc::clone(map.entry(name.to_string()).or_default()))
    }

    /// Drop a slot that holds nothing, so probing unknown names cannot grow the map.
    fn forget_empty(&self, name: &str, slot: &Slot) {
        if let Ok(mut map) = self.indexes.lock() {
            let empty = slot.lock().map(|g| g.is_none()).unwrap_or(false);
            if empty && map.get(name).is_some_and(|s| Arc::ptr_eq(s, slot)) {
                map.remove(name);
            }
        }
    }

    /// Forget an entry whose writer stopped, so the next request reopens it.
    fn evict(&self, name: &str, entry: &Arc<Entry>) {
        if let Ok(slot) = self.slot(name)
            && let Ok(mut guard) = slot.lock()
            && guard.as_ref().is_some_and(|e| Arc::ptr_eq(e, entry))
        {
            *guard = None;
        }
    }

    /// The open index, rebuilding it from the store on first use after a restart.
    fn entry(&self, name: &str) -> Result<Arc<Entry>, ApiError> {
        valid_index(name)?;
        let slot = self.slot(name)?;
        let opened = {
            let mut guard = slot.lock().map_err(|_| poisoned())?;
            if let Some(entry) = &*guard {
                return Ok(Arc::clone(entry));
            }
            self.open(name)
                .inspect(|entry| *guard = Some(Arc::clone(entry)))
        };
        if opened.is_err() {
            self.forget_empty(name, &slot);
        }
        opened
    }

    fn mapping_key(hash: &Digest) -> String {
        Layout::new(DEPLOYMENT)
            .expect("the deployment name is valid")
            .mapping(hash)
    }

    fn cached(&self, hash: &Digest) -> Option<Arc<Mapping>> {
        let shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        shared.get(hash).map(Arc::clone)
    }

    fn remember(&self, hash: Digest, mapping: &Arc<Mapping>) {
        let mut shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        if shared.len() < MAX_SHARED_MAPPINGS {
            shared.insert(hash, Arc::clone(mapping));
        }
    }

    /// Store a mapping as one immutable object under its content hash and return
    /// the node's shared copy with its hash. Identical mappings write the object
    /// once; a node that has already stored or verified it writes nothing.
    fn share(&self, mapping: Mapping) -> Result<(Arc<Mapping>, Digest), ApiError> {
        let hash = mapping.hash();
        if let Some(found) = self.cached(&hash) {
            return Ok((found, hash));
        }
        hsearch_index::put_shared(
            &*self.store,
            &Self::mapping_key(&hash),
            &mapping.to_object()?,
        )?;
        let mapping = Arc::new(mapping);
        self.remember(hash, &mapping);
        Ok((mapping, hash))
    }

    /// An index's mapping, resolved only through that index's own stored
    /// reference. Mappings an earlier node wrote inline are read as they are.
    fn load_mapping(&self, name: &str) -> Result<(Arc<Mapping>, Vec<u8>), ApiError> {
        let stored = hsearch_index::get_mapping(&*self.store, name)?;
        if Mapping::is_inline(&stored) {
            let mapping = Arc::new(Mapping::decode(&stored)?);
            return Ok((mapping, stored));
        }
        let corrupt = |reason| ApiError::new(500, "corrupt_exception", reason);
        let hash = Mapping::parse_reference(&stored)
            .ok_or_else(|| corrupt("stored mapping reference is unreadable"))?;
        if let Some(found) = self.cached(&hash) {
            return Ok((found, stored));
        }
        let object = hsearch_index::get_shared(&*self.store, &Self::mapping_key(&hash)).map_err(
            |error| match error {
                hsearch_index::Error::NotFound => {
                    corrupt("the shared mapping an index names is missing")
                }
                other => ApiError::from(other),
            },
        )?;
        let mapping = Arc::new(Mapping::from_object(&object, &hash)?);
        self.remember(hash, &mapping);
        Ok((mapping, stored))
    }

    /// The engine schema for `mapping` under this node's settings.
    fn schema_of(&self, mapping: &Mapping) -> Result<hsearch_engine::Schema, ApiError> {
        Ok(mapping
            .schema()?
            .with_auto_merge_mb(self.limits.auto_merge_mb))
    }

    fn open(&self, name: &str) -> Result<Arc<Entry>, ApiError> {
        let (mapping, stored) = self.load_mapping(name)?;
        let index = ServedIndex::open(
            &self.store,
            &self.settings(),
            name,
            self.schema_of(&mapping)?,
            &mapping.hash(),
            &self.scratch(name),
        )?;
        Ok(Arc::new(Entry::new(index, mapping, stored)))
    }

    /// `PUT /{index}`. The root is created first and the mapping second; a
    /// retry that finds a root without a mapping finishes the creation.
    pub fn create(&self, name: &str, body: &Value) -> Result<Value, ApiError> {
        valid_index(name)?;
        let mapping = Mapping::parse_with(body, &self.mapping_limits)?;
        let schema = self.schema_of(&mapping)?;
        let slot = self.slot(name)?;
        let created = {
            let mut guard = slot.lock().map_err(|_| poisoned())?;
            if guard.is_some() {
                return Err(hsearch_index::Error::Exists.into());
            }
            let hash = mapping.hash();
            let settings = self.settings();
            let index = match ServedIndex::create(
                &self.store,
                &settings,
                name,
                schema.clone(),
                &hash,
                &self.scratch(name),
            ) {
                Ok(index) => Ok(index),
                Err(hsearch_index::Error::Exists) => {
                    match hsearch_index::get_mapping(&*self.store, name) {
                        Err(hsearch_index::Error::NotFound) => ServedIndex::open(
                            &self.store,
                            &settings,
                            name,
                            schema,
                            &hash,
                            &self.scratch(name),
                        ),
                        _ => Err(hsearch_index::Error::Exists),
                    }
                }
                Err(error) => Err(error),
            };
            index.map_err(ApiError::from).and_then(|index| {
                let (mapping, hash) = self.share(mapping.clone())?;
                let stored = Mapping::reference(&hash);
                hsearch_index::put_mapping(&*self.store, name, &stored)?;
                *guard = Some(Arc::new(Entry::new(index, mapping, stored)));
                Ok(())
            })
        };
        if created.is_err() {
            self.forget_empty(name, &slot);
        }
        created?;
        Ok(json!({"acknowledged": true, "shards_acknowledged": true, "index": name}))
    }

    /// `PUT /{index}/_mapping`: add fields to a live index, or change its dynamic
    /// setting, as a new immutable mapping version. Writes and searches keep running:
    /// every document is built entirely under the version before or the one after.
    /// A field redeclared with another type is refused and nothing changes.
    pub fn put_mapping(&self, name: &str, body: &Value) -> Result<Value, ApiError> {
        let entry = self.entry(name)?;
        let current = entry.mapping();
        // Refuse before taking the change lock: a refusal costs no request.
        if current.evolve(body, &self.mapping_limits)?.is_none() {
            return Ok(json!({"acknowledged": true}));
        }
        let mut stored = entry.stored.lock().map_err(|_| poisoned())?;
        // Read again under the lock: another change may have landed first, and this
        // one applies to the version that is current now.
        let current = entry.mapping();
        if let Some(next) = current.evolve(body, &self.mapping_limits)? {
            self.publish(name, &entry, &current, next, &mut stored)?;
        }
        Ok(json!({"acknowledged": true}))
    }

    /// Publish `next` as the mapping of `name` and move the live index and its
    /// readers to it, in that order. Requests, however many documents or segments
    /// the index has: one create-only PUT of the immutable mapping object (none if
    /// this node already holds it), and a HEAD, a GET and one compare-exchange of
    /// the index's reference.
    fn publish(
        &self,
        name: &str,
        entry: &Arc<Entry>,
        current: &Mapping,
        next: Mapping,
        stored: &mut Vec<u8>,
    ) -> Result<(), ApiError> {
        let schema = self.schema_of(&next)?;
        let grew = next.fields.len() > current.fields.len();
        let (next, hash) = self.share(next)?;
        let reference = Mapping::reference(&hash);
        if let Err(error) = hsearch_index::replace_mapping(&*self.store, name, stored, &reference) {
            // Another writer changed the mapping, or whether this write landed is not
            // known: forget the index, so the next request reopens it under whatever
            // the store now holds.
            if matches!(
                error,
                hsearch_index::Error::Changed | hsearch_index::Error::Unknown
            ) {
                self.evict(name, entry);
            }
            return Err(error.into());
        }
        *stored = reference;
        // From here the new version is durable. The index evolves before the mapping
        // does, so no document is built with a field the writer cannot yet take.
        if grew && let Err(error) = entry.index.evolve(schema, &hash) {
            self.evict(name, entry);
            return Err(error.into());
        }
        *entry.mapping.write().map_err(|_| poisoned())? = next;
        Ok(())
    }

    /// Under `dynamic: true`, add the fields a run of documents introduces as one
    /// new mapping version, however many documents or fields: so the cost of a
    /// bulk that adds fields is one mapping change, not one per document. A
    /// document whose fields cannot be added (a type another document fixed first,
    /// an unmappable value, a limit) is refused alone by [`document`].
    fn add_dynamic_fields(&self, name: &str, entry: &Arc<Entry>, window: &[Item]) {
        let mapping = entry.mapping();
        if mapping.dynamic != Dynamic::True {
            return;
        }
        let mut pending: Vec<(String, Kind)> = Vec::new();
        // Every item of the batch for this index (a batch is at most `CHUNK` items,
        // so this covers the whole run), skipping those refused regardless: a
        // document that will not be written must not add a field.
        for item in window.iter().take_while(|it| it.index == name) {
            if check_id(&item.id).is_err() {
                continue;
            }
            if let Ok((Action::Index, Some(Value::Object(object)))) = &item.work {
                if self.mapping_limits.check_source(object).is_err() {
                    continue;
                }
                let _ = plan_dynamic(&mapping, &self.mapping_limits, object, &mut pending);
            }
        }
        if pending.is_empty() {
            return;
        }
        let Ok(mut stored) = entry.stored.lock() else {
            return;
        };
        // Under the lock the current version may already have some of these.
        let current = entry.mapping();
        let mut fresh = Vec::new();
        plan_pending(&current, &self.mapping_limits, pending, &mut fresh);
        if fresh.is_empty() {
            return;
        }
        if let Ok(next) = current.next_version(fresh, current.dynamic, &self.mapping_limits) {
            // A failure leaves the version as it was; the documents that needed the
            // fields are refused with the reason `document` finds.
            let _ = self.publish(name, entry, &current, next, &mut stored);
        }
    }

    /// One write, bisecting a refused chunk so a bad item fails alone. A single
    /// operation cannot be bisected, so it is moved, not cloned: the common large
    /// document is never copied here.
    fn write(
        &self,
        name: &str,
        entry: &Arc<Entry>,
        mut ops: Vec<Op>,
        wait: Wait,
    ) -> Vec<Result<hsearch_index::ItemResult, ApiError>> {
        let n = ops.len();
        let attempt = if n > 1 {
            entry.index.bulk(ops.clone(), wait)
        } else {
            entry.index.bulk(std::mem::take(&mut ops), wait)
        };
        match attempt {
            Ok(results) => results
                .into_iter()
                .map(|item| item.map_err(ApiError::from))
                .collect(),
            Err(hsearch_index::Error::Invalid) if n > 1 => {
                let right = ops.split_off(n / 2);
                let mut out = self.write(name, entry, ops, wait);
                out.extend(self.write(name, entry, right, wait));
                out
            }
            Err(error) => {
                if stopped(&error) {
                    self.evict(name, entry);
                }
                vec![Err(ApiError::from(error)); n]
            }
        }
    }

    /// `POST /_bulk` and `POST /{index}/_bulk`, streamed: the body is read one
    /// item at a time and written in batches bounded by the buffer, so memory
    /// does not grow with the body. `body_bytes` is the declared length, if any.
    ///
    /// A broken stream or a limit crossed mid-body refuses the request. Batches
    /// already written stay written, and the error body carries their per-item
    /// results in `items`; a body that fails inside its first batch commits nothing.
    pub fn bulk(
        &self,
        body: impl std::io::Read,
        body_bytes: Option<u64>,
        default_index: Option<&str>,
        wait: Wait,
    ) -> Result<(u16, Value), ApiError> {
        let started = Instant::now();
        let _permit = self
            .gate
            .reserve(self.limits.request_cost(body_bytes.unwrap_or(u64::MAX)))?;
        let mut stream = bulk::Stream::new(body, self.limits).with_arrays(true);
        let flush_bytes = self.limits.buffer_bytes / 2;
        let mut batch: Vec<Item> = Vec::new();
        let mut pending = 0;
        let mut responses: Vec<Value> = Vec::new();
        let mut errors = false;
        loop {
            match stream.next(default_index) {
                Ok(Some(item)) => {
                    pending += item.bytes;
                    batch.push(item);
                }
                Ok(None) => break,
                Err(mut error) => {
                    if responses.is_empty() {
                        return Err(error);
                    }
                    // Earlier batches are written: say which items they were, so
                    // the client knows what succeeded and what to resend.
                    error.reason = format!(
                        "{}; {} earlier items were already applied",
                        error.reason,
                        responses.len()
                    );
                    return Ok((
                        error.status,
                        json!({"error": error.object(), "status": error.status,
                               "took": started.elapsed().as_millis() as u64,
                               "errors": true, "items": responses}),
                    ));
                }
            }
            if batch.len() >= CHUNK || pending >= flush_bytes {
                errors |= self.charged_batch(&mut batch, wait, &mut responses);
                pending = 0;
            }
        }
        errors |= self.charged_batch(&mut batch, wait, &mut responses);
        if responses.is_empty() {
            return Err(ApiError::bad("the bulk body has no actions"));
        }
        Ok((
            200,
            json!({
                "took": started.elapsed().as_millis() as u64,
                "errors": errors,
                "items": responses,
            }),
        ))
    }

    /// [`Self::run_batch`] after reserving the indexing working set of the batch's
    /// large documents. When the budget cannot cover it the whole batch is refused
    /// item by item with 429 and nothing is written: visible backpressure, no queue.
    fn charged_batch(&self, items: &mut Vec<Item>, wait: Wait, out: &mut Vec<Value>) -> bool {
        let cost: u64 = items
            .iter()
            .filter(|item| item.bytes >= LARGE_DOCUMENT_BYTES)
            .map(|item| INDEX_COST_FACTOR.saturating_mul(item.bytes as u64))
            .sum();
        let _held = if cost == 0 {
            None
        } else {
            match self.gate.reserve(cost) {
                Ok(permit) => Some(permit),
                Err(error) => {
                    out.extend(
                        items
                            .drain(..)
                            .map(|item| item_error(&item.action, &item.index, &item.id, &error)),
                    );
                    return true;
                }
            }
        };
        self.run_batch(items, wait, out)
    }

    /// Write one batch, appending a response per item in order and emptying the
    /// batch. Returns whether any item failed.
    fn run_batch(&self, items: &mut Vec<Item>, wait: Wait, out: &mut Vec<Value>) -> bool {
        let mut errors = false;
        let mut responses: Vec<Option<Value>> = vec![None; items.len()];
        let mut i = 0;
        while i < items.len() {
            // A run of consecutive valid items for one index is one write.
            let name = items[i].index.clone();
            let entry = valid_index(&name).and_then(|()| self.entry(&name));
            if let Ok(entry) = &entry {
                self.add_dynamic_fields(&name, entry, &items[i..]);
            }
            // One mapping version for the whole run, read after any fields the run
            // adds: a document is built entirely under one version.
            let mapping = entry.as_ref().ok().map(|entry| entry.mapping());
            let mut ops: Vec<Op> = Vec::new();
            let mut slots: Vec<usize> = Vec::new();
            let mut j = i;
            while j < items.len() && items[j].index == name && ops.len() < CHUNK {
                let item = &mut items[j];
                let work = std::mem::replace(&mut item.work, Ok((Action::Delete, None)));
                let verdict = match (&entry, work) {
                    (Err(e), _) => Err(e.clone()),
                    (_, Err(e)) => Err(e),
                    (Ok(_), Ok(_)) if check_id(&item.id).is_err() => {
                        Err(check_id(&item.id).unwrap_err())
                    }
                    (Ok(_), Ok((Action::Delete, _))) => Ok(Op::Delete {
                        id: item.id.clone(),
                    }),
                    (Ok(_), Ok((Action::Index, Some(source)))) => match &mapping {
                        Some(mapping) => document(mapping, &self.mapping_limits, &item.id, source),
                        None => Err(poisoned()),
                    },
                    (Ok(_), Ok((Action::Index, None))) => Err(ApiError::bad("missing source")),
                };
                match verdict {
                    Ok(op) => {
                        ops.push(op);
                        slots.push(j);
                    }
                    Err(error) => {
                        errors = true;
                        responses[j] = Some(item_error(&item.action, &name, &item.id, &error));
                    }
                }
                j += 1;
            }
            if let (Ok(entry), false) = (&entry, ops.is_empty()) {
                for (slot, result) in slots.iter().zip(self.write(&name, entry, ops, wait)) {
                    errors |= result.is_err();
                    responses[*slot] = Some(match result {
                        Ok(result) => item_result(&items[*slot].action, &name, &result),
                        Err(error) => {
                            item_error(&items[*slot].action, &name, &items[*slot].id, &error)
                        }
                    });
                }
            }
            i = j;
        }
        items.clear();
        out.extend(responses.into_iter().flatten());
        errors
    }

    /// `POST /{index}/_search`.
    pub fn search(&self, name: &str, body: &Value) -> Result<Value, ApiError> {
        let started = Instant::now();
        let entry = self.entry(name)?;
        let mapping = entry.mapping();
        let parsed = search::parse(&mapping, body)?;
        let projection: Vec<&str> = match &parsed.source {
            Source::All => mapping.names(),
            Source::None => Vec::new(),
            Source::Some(names) => names.iter().map(String::as_str).collect(),
        };
        let (queries, values) = (Arena::new(), Arena::new());
        let query = search::lower(&parsed.query, &queries, &values);
        let sort = search::engine_sort(&parsed.sort);
        let snapshot = entry.index.snapshot()?;
        let result =
            snapshot
                .reader
                .search(&query, &sort, parsed.from + parsed.size, &projection)?;
        let hits: Vec<Value> = result
            .hits
            .iter()
            .skip(parsed.from)
            .map(|hit| {
                let mut out = Map::new();
                out.insert("_index".into(), json!(name));
                out.insert("_id".into(), json!(hit.id));
                out.insert("_score".into(), Value::Null);
                if parsed.source != Source::None {
                    let mut source = Map::new();
                    for (field, value) in &hit.fields {
                        source.insert(field.clone(), json_value(value));
                    }
                    out.insert("_source".into(), Value::Object(source));
                }
                Value::Object(out)
            })
            .collect();
        Ok(json!({
            "took": started.elapsed().as_millis() as u64,
            "timed_out": false,
            "hits": {"total": {"value": result.total, "relation": "eq"},
                     "max_score": null, "hits": hits},
        }))
    }

    /// `POST /{index}/_refresh`.
    pub fn refresh(&self, name: &str) -> Result<Value, ApiError> {
        self.entry(name)?.index.refresh()?;
        Ok(json!({"_shards": {"total": 1, "successful": 1, "failed": 0}}))
    }
}

/// Build the write path's operation from a source object, in schema order. The
/// source is consumed: its strings move into the operation instead of being copied.
fn document(
    mapping: &Mapping,
    limits: &MappingLimits,
    id: &str,
    source: Value,
) -> Result<Op, ApiError> {
    let Value::Object(mut object) = source else {
        return Err(ApiError::new(
            400,
            "mapper_parsing_exception",
            "the source must be a JSON object",
        ));
    };
    limits.check_source(&object)?;
    // Counting first keeps the common case, no unknown field, to one pass over
    // the keys that the lookups below already make.
    let unknown = object.keys().filter(|k| mapping.kind(k).is_none()).count();
    if unknown > 0 {
        match mapping.dynamic {
            Dynamic::Strict => {
                let extra = object.keys().find(|k| mapping.kind(k).is_none());
                return Err(ApiError::new(
                    400,
                    "strict_dynamic_mapping_exception",
                    format!(
                        "mapping is set to strict: dynamic introduction of [{}] is not allowed",
                        extra.map_or("", String::as_str)
                    ),
                ));
            }
            Dynamic::False => object.retain(|k, _| mapping.kind(k).is_some()),
            Dynamic::True => {
                // A null is no value: it adds no field and is not indexed.
                object.retain(|k, v| mapping.kind(k).is_some() || !v.is_null());
                if object.keys().any(|k| mapping.kind(k).is_none()) {
                    // The run's added fields did not cover this document: say why.
                    return Err(plan_dynamic(mapping, limits, &object, &mut Vec::new())
                        .err()
                        .unwrap_or_else(|| {
                            ApiError::new(
                                503,
                                "mapping_update_unavailable_exception",
                                "the mapping could not take this document's new fields; retry it",
                            )
                        }));
                }
            }
        }
    }
    let required = mapping.required();
    let mut fields = Vec::with_capacity(mapping.fields.len());
    for (at, (name, kind)) in mapping.fields.iter().enumerate() {
        let value = match object.remove(name) {
            // A field added after the index was created may be left out, or null.
            None | Some(Value::Null) if at >= required => {
                fields.push(Field::Absent);
                continue;
            }
            Some(value) => value,
            None => {
                return Err(ApiError::new(
                    400,
                    "mapper_parsing_exception",
                    format!("field [{name}] is required: every mapped field must be present"),
                ));
            }
        };
        fields.push(typed_owned(*kind, value).map_err(|why| {
            ApiError::new(
                400,
                "mapper_parsing_exception",
                format!("field [{name}]: {why}"),
            )
        })?);
    }
    let _: &[Field] = &fields;
    Ok(Op::Put {
        id: id.to_string(),
        fields,
    })
}

/// Add to `pending` the fields `object` introduces that `mapping` and `pending`
/// do not already have, each with the kind its first value maps to, or say why the
/// document cannot be mapped: a value that cannot be inferred, a field already
/// fixed as another kind, a name the engine refuses, or a limit. All or nothing: a
/// refused document adds none of its fields.
fn plan_dynamic(
    mapping: &Mapping,
    limits: &MappingLimits,
    object: &Map<String, Value>,
    pending: &mut Vec<(String, Kind)>,
) -> Result<(), ApiError> {
    let mut fresh: Vec<(String, Kind)> = Vec::new();
    for (name, value) in object {
        if value.is_null() && mapping.kind(name).is_none() {
            continue;
        }
        let known = mapping
            .kind(name)
            .or_else(|| pending.iter().find(|(n, _)| n == name).map(|(_, k)| *k));
        let Some(known) = known else {
            if !valid_field_name(name) {
                return Err(ApiError::new(
                    400,
                    "mapper_parsing_exception",
                    format!(
                        "field [{name}]: names are 1-64 of [A-Za-z0-9_] and do not start with an underscore"
                    ),
                ));
            }
            fresh.push((name.clone(), infer(name, value)?));
            continue;
        };
        // A field another document fixed in this run fixes the kind for this one.
        if pending.iter().any(|(n, _)| n == name) {
            typed_owned(known, value.clone()).map_err(|why| {
                ApiError::new(
                    400,
                    "mapper_parsing_exception",
                    format!("field [{name}]: {why}"),
                )
            })?;
        }
    }
    let total = mapping.fields.len() + pending.len() + fresh.len();
    limits.check_fields(total)?;
    // The engine's own bound sits under the configured one.
    if total + 2 > hsearch_engine::MAX_FIELDS {
        return Err(ApiError::new(
            400,
            "mapper_parsing_exception",
            format!(
                "an index holds at most {} fields",
                hsearch_engine::MAX_FIELDS - 2
            ),
        ));
    }
    pending.extend(fresh);
    Ok(())
}

/// The part of `pending` the current mapping still lacks, with the kind any
/// other change already gave an existing field left alone.
fn plan_pending(
    mapping: &Mapping,
    limits: &MappingLimits,
    pending: Vec<(String, Kind)>,
    fresh: &mut Vec<(String, Kind)>,
) {
    for (name, kind) in pending {
        if mapping.kind(&name).is_none() {
            fresh.push((name, kind));
        }
    }
    // Keep only what fits; the documents that needed the rest are refused with the
    // limit that stopped them.
    let room = limits
        .max_fields
        .min(hsearch_engine::MAX_FIELDS - 2)
        .saturating_sub(mapping.fields.len());
    fresh.truncate(room);
}

fn item_error(action: &str, index: &str, id: &str, error: &ApiError) -> Value {
    json!({action: {"_index": index, "_id": id, "status": error.status, "error": error.object()}})
}

fn item_result(action: &str, index: &str, result: &hsearch_index::ItemResult) -> Value {
    let (text, status) = match result.outcome {
        Outcome::Created => ("created", 201),
        Outcome::Updated => ("updated", 200),
        Outcome::Deleted => ("deleted", 200),
        Outcome::NotFound => ("not_found", 404),
    };
    json!({action: {"_index": index, "_id": result.id, "_version": result.version,
                    "result": text, "status": status}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use hsearch_object::memory::{MemoryLimits, MemoryStore};

    fn store() -> Arc<MemoryStore> {
        Arc::new(
            MemoryStore::new(MemoryLimits {
                objects: 4096,
                versions: 16384,
                version_bytes: 64 << 20,
                identities: 16384,
                identity_bytes: 64 << 20,
            })
            .unwrap(),
        )
    }

    fn dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hsearch-node-unit-{label}-{}", std::process::id()))
    }

    #[test]
    fn the_node_hands_its_auto_merge_setting_to_every_index_schema() {
        let body = json!({"mappings": {"properties": {"a": {"type": "keyword"}}}});
        let mapping = Mapping::parse_with(&body, &MappingLimits::default()).unwrap();
        let schema = |mb: usize| {
            let limits = Limits {
                auto_merge_mb: mb,
                ..Limits::default()
            };
            let node = Node::with_limits(store(), dir("merge"), limits).unwrap();
            node.schema_of(&mapping).unwrap().limits().auto_merge_mb
        };
        assert_eq!(Node::new(store(), dir("merge")).limits().auto_merge_mb, 4);
        assert_eq!((schema(0), schema(64), schema(16)), (0, 64, 16));
        // The engine clamps what the node would never send.
        assert_eq!(
            hsearch_engine::Schema::new(1, &[("a", hsearch_engine::Kind::Keyword)])
                .unwrap()
                .with_auto_merge_mb(usize::MAX)
                .limits()
                .auto_merge_mb,
            hsearch_engine::MAX_AUTO_MERGE_MB
        );
    }

    #[test]
    fn indexes_with_one_mapping_hold_one_parsed_copy_across_a_restart() {
        let (store, one, two) = (store(), dir("copy1"), dir("copy2"));
        let body = json!({"mappings": {"properties": {"a": {"type": "keyword"}}}});
        let other = json!({"mappings": {"properties": {"b": {"type": "keyword"}}}});
        let node = Node::new(Arc::clone(&store), one.clone());
        for name in ["a", "b", "c"] {
            node.create(name, &body).unwrap();
        }
        node.create("d", &other).unwrap();
        let held = |node: &Node<MemoryStore>| {
            ["a", "b", "c", "d"].map(|n| node.entry(n).unwrap().mapping())
        };
        let before = held(&node);
        assert!(Arc::ptr_eq(&before[0], &before[1]) && Arc::ptr_eq(&before[1], &before[2]));
        assert!(!Arc::ptr_eq(&before[0], &before[3]));
        assert_eq!(
            node.shared_mapping_count(),
            2,
            "one parsed copy per distinct mapping"
        );
        // After a restart the indexes open from their references and share again.
        let restarted = Node::new(store, two.clone());
        let after = held(&restarted);
        assert!(Arc::ptr_eq(&after[0], &after[1]) && Arc::ptr_eq(&after[1], &after[2]));
        assert_eq!(restarted.shared_mapping_count(), 2);
        let _ = (std::fs::remove_dir_all(one), std::fs::remove_dir_all(two));
    }

    /// Hot-path cost of building a write from a typical five-field source, with
    /// and without the limit and dynamic checks. Run:
    /// `cargo test --release -p hsearch-node document_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn document_cost() {
        let mapping = Mapping::parse(&json!({"mappings": {"properties": {
            "title": {"type": "text"}, "tag": {"type": "keyword"}, "n": {"type": "integer"},
            "ok": {"type": "boolean"}, "at": {"type": "date"}}}}))
        .unwrap();
        let source =
            json!({"title": "a short title here", "tag": "x", "n": 5, "ok": true, "at": 1000});
        let limits = MappingLimits::default();
        let rounds = 2_000_000u32;
        let time = |f: &dyn Fn()| {
            let start = Instant::now();
            for _ in 0..rounds {
                f();
            }
            start.elapsed().as_nanos() as f64 / f64::from(rounds)
        };
        let whole = time(&|| {
            std::hint::black_box(document(&mapping, &limits, "id", source.clone()).unwrap());
        });
        let clone_only = time(&|| {
            std::hint::black_box(source.clone());
        });
        let checks = time(&|| {
            let object = source.as_object().unwrap();
            limits.check_source(object).unwrap();
            std::hint::black_box(object.keys().filter(|k| mapping.kind(k).is_none()).count());
        });
        println!(
            "document() {whole:.0} ns (of which the source clone {clone_only:.0} ns); \
                  the new checks alone {checks:.0} ns"
        );
    }
}
