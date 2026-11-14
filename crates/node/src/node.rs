//! The node's operations over indexes held in memory and rebuilt from the
//! object store: create, bulk, get, search and refresh.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hsearch_engine::{Outcome, StoredValue};
use hsearch_format::{Digest, Layout};
use hsearch_index::{Field, Index, Op, Wait};
use hsearch_object::MetadataStore;
use serde_json::{Map, Value, json};
use typed_arena::Arena;

use crate::ApiError;
use crate::bulk::{self, Action, Item};
use crate::limits::{Limits, MemoryGate};
use crate::mapping::{Mapping, typed_owned};
use crate::search::{self, Source};

/// Operations per call to the write path, below its slot bound.
const CHUNK: usize = 1000;

struct Entry {
    index: Index,
    /// One parsed copy per node, shared by every index with this mapping.
    mapping: Arc<Mapping>,
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
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(ApiError::new(
            400,
            "invalid_index_name_exception",
            "index names are 1-64 of [a-z0-9_-]",
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

impl<S: MetadataStore + 'static> Node<S> {
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
        })
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
    fn load_mapping(&self, name: &str) -> Result<Arc<Mapping>, ApiError> {
        let stored = hsearch_index::get_mapping(&*self.store, name)?;
        if Mapping::is_inline(&stored) {
            return Ok(Arc::new(Mapping::decode(&stored)?));
        }
        let corrupt = |reason| ApiError::new(500, "corrupt_exception", reason);
        let hash = Mapping::parse_reference(&stored)
            .ok_or_else(|| corrupt("stored mapping reference is unreadable"))?;
        if let Some(found) = self.cached(&hash) {
            return Ok(found);
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
        Ok(mapping)
    }

    fn open(&self, name: &str) -> Result<Arc<Entry>, ApiError> {
        let mapping = self.load_mapping(name)?;
        let index = Index::open(
            Arc::clone(&self.store),
            name,
            mapping.schema()?,
            &self.scratch(name),
        )?;
        Ok(Arc::new(Entry { index, mapping }))
    }

    /// `PUT /{index}`. The root is created first and the mapping second; a
    /// retry that finds a root without a mapping finishes the creation.
    pub fn create(&self, name: &str, body: &Value) -> Result<Value, ApiError> {
        valid_index(name)?;
        let mapping = Mapping::parse(body)?;
        let schema = mapping.schema()?;
        let slot = self.slot(name)?;
        let created = {
            let mut guard = slot.lock().map_err(|_| poisoned())?;
            if guard.is_some() {
                return Err(hsearch_index::Error::Exists.into());
            }
            let index = match Index::create(
                Arc::clone(&self.store),
                name,
                schema.clone(),
                &self.scratch(name),
            ) {
                Ok(index) => Ok(index),
                Err(hsearch_index::Error::Exists) => {
                    match hsearch_index::get_mapping(&*self.store, name) {
                        Err(hsearch_index::Error::NotFound) => {
                            Index::open(Arc::clone(&self.store), name, schema, &self.scratch(name))
                        }
                        _ => Err(hsearch_index::Error::Exists),
                    }
                }
                Err(error) => Err(error),
            };
            index.map_err(ApiError::from).and_then(|index| {
                let (mapping, hash) = self.share(mapping.clone())?;
                hsearch_index::put_mapping(&*self.store, name, &Mapping::reference(&hash))?;
                *guard = Some(Arc::new(Entry { index, mapping }));
                Ok(())
            })
        };
        if created.is_err() {
            self.forget_empty(name, &slot);
        }
        created?;
        Ok(json!({"acknowledged": true, "shards_acknowledged": true, "index": name}))
    }

    /// One write, bisecting a refused chunk so a bad item fails alone.
    fn write(
        &self,
        name: &str,
        entry: &Arc<Entry>,
        ops: &[Op],
        wait: Wait,
    ) -> Vec<Result<hsearch_index::ItemResult, ApiError>> {
        match entry.index.bulk(ops.to_vec(), wait) {
            Ok(results) => results.into_iter().map(Ok).collect(),
            Err(hsearch_index::Error::Invalid) if ops.len() > 1 => {
                let (left, right) = ops.split_at(ops.len() / 2);
                let mut out = self.write(name, entry, left, wait);
                out.extend(self.write(name, entry, right, wait));
                out
            }
            Err(error) => {
                if stopped(&error) {
                    self.evict(name, entry);
                }
                vec![Err(ApiError::from(error)); ops.len()]
            }
        }
    }

    /// `POST /_bulk` and `POST /{index}/_bulk`, streamed: the body is read one
    /// item at a time and written in batches bounded by the buffer, so memory
    /// does not grow with the body. `body_bytes` is the declared length, if any.
    ///
    /// A structurally broken stream refuses the request. Batches already written
    /// stay written and the error says how many items that was; a body that
    /// breaks inside its first batch commits nothing.
    pub fn bulk(
        &self,
        body: impl std::io::Read,
        body_bytes: Option<u64>,
        default_index: Option<&str>,
        wait: Wait,
    ) -> Result<Value, ApiError> {
        let started = Instant::now();
        let _permit = self
            .gate
            .reserve(self.limits.request_cost(body_bytes.unwrap_or(u64::MAX)))?;
        let mut stream = bulk::Stream::new(body, self.limits);
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
                    if !responses.is_empty() {
                        error.reason = format!(
                            "{}; {} earlier items were already applied",
                            error.reason,
                            responses.len()
                        );
                    }
                    return Err(error);
                }
            }
            if batch.len() >= CHUNK || pending >= flush_bytes {
                errors |= self.run_batch(&mut batch, wait, &mut responses);
                pending = 0;
            }
        }
        errors |= self.run_batch(&mut batch, wait, &mut responses);
        if responses.is_empty() {
            return Err(ApiError::bad("the bulk body has no actions"));
        }
        Ok(json!({
            "took": started.elapsed().as_millis() as u64,
            "errors": errors,
            "items": responses,
        }))
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
                    (Ok(entry), Ok((Action::Index, Some(source)))) => {
                        document(&entry.mapping, &item.id, source)
                    }
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
                for (slot, result) in slots.iter().zip(self.write(&name, entry, &ops, wait)) {
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

    /// `GET /{index}/_doc/{id}`, from the latest searchable view.
    pub fn get(&self, name: &str, id: &str) -> Result<(u16, Value), ApiError> {
        let entry = self.entry(name)?;
        let names = entry.mapping.names();
        let snapshot = entry.index.snapshot()?;
        match snapshot.get(id, &names)? {
            None => Ok((404, json!({"_index": name, "_id": id, "found": false}))),
            Some(hit) => {
                let mut source = Map::new();
                for (field, value) in &hit.fields {
                    source.insert(field.clone(), json_value(value));
                }
                Ok((
                    200,
                    json!({"_index": name, "_id": id, "_version": hit.version,
                           "found": true, "_source": source}),
                ))
            }
        }
    }

    /// `POST /{index}/_search`.
    pub fn search(&self, name: &str, body: &Value) -> Result<Value, ApiError> {
        let started = Instant::now();
        let entry = self.entry(name)?;
        let parsed = search::parse(&entry.mapping, body)?;
        let projection: Vec<&str> = match &parsed.source {
            Source::All => entry.mapping.names(),
            Source::None => Vec::new(),
            Source::Some(names) => names.iter().map(String::as_str).collect(),
        };
        let (queries, values) = (Arena::new(), Arena::new());
        let query = search::lower(&parsed.query, &queries, &values);
        let sort = search::engine_sort(&parsed.sort);
        let snapshot = entry.index.snapshot()?;
        let result = snapshot.search(&query, &sort, parsed.from + parsed.size, &projection)?;
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
fn document(mapping: &Mapping, id: &str, source: Value) -> Result<Op, ApiError> {
    let Value::Object(mut object) = source else {
        return Err(ApiError::new(
            400,
            "mapper_parsing_exception",
            "the source must be a JSON object",
        ));
    };
    if let Some(extra) = object.keys().find(|k| mapping.kind(k).is_none()) {
        return Err(ApiError::new(
            400,
            "strict_dynamic_mapping_exception",
            format!("unknown field [{extra}]; this node has no dynamic mapping"),
        ));
    }
    let mut fields = Vec::with_capacity(mapping.fields.len());
    for (name, kind) in &mapping.fields {
        let value = object.remove(name).ok_or_else(|| {
            ApiError::new(
                400,
                "mapper_parsing_exception",
                format!("field [{name}] is required: every mapped field must be present"),
            )
        })?;
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
            ["a", "b", "c", "d"].map(|n| node.entry(n).unwrap().mapping.clone())
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
}
