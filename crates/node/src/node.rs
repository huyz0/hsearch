//! The node's operations over indexes held in memory and rebuilt from the
//! object store: create, bulk, get, search and refresh.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hsearch_engine::{Outcome, StoredValue};
use hsearch_index::{Field, Index, Op, Wait};
use hsearch_object::MetadataStore;
use serde_json::{Map, Value, json};
use typed_arena::Arena;

use crate::ApiError;
use crate::bulk::{self, Action};
use crate::mapping::{Mapping, typed};
use crate::search::{self, Source};

/// Operations per call to the write path, below its slot bound.
const CHUNK: usize = 1000;

struct Entry {
    index: Index,
    mapping: Mapping,
}

/// One node: a store, a scratch directory and the indexes opened so far.
pub struct Node<S> {
    store: Arc<S>,
    data_dir: PathBuf,
    indexes: Mutex<HashMap<String, Arc<Entry>>>,
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
    /// A node over `store`, keeping scratch engines under `data_dir`.
    pub fn new(store: Arc<S>, data_dir: PathBuf) -> Self {
        Self {
            store,
            data_dir,
            indexes: Mutex::new(HashMap::new()),
        }
    }

    fn scratch(&self, name: &str) -> PathBuf {
        self.data_dir.join(name)
    }

    /// The open index, rebuilding it from the store on first use after a restart.
    fn entry(&self, name: &str) -> Result<Arc<Entry>, ApiError> {
        valid_index(name)?;
        let mut indexes = self
            .indexes
            .lock()
            .map_err(|_| ApiError::new(500, "internal_exception", "node state poisoned"))?;
        if let Some(entry) = indexes.get(name) {
            return Ok(Arc::clone(entry));
        }
        let mapping = Mapping::decode(&hsearch_index::get_mapping(&*self.store, name)?)?;
        let index = Index::open(
            Arc::clone(&self.store),
            name,
            mapping.schema()?,
            &self.scratch(name),
        )?;
        let entry = Arc::new(Entry { index, mapping });
        indexes.insert(name.to_string(), Arc::clone(&entry));
        Ok(entry)
    }

    /// `PUT /{index}`.
    pub fn create(&self, name: &str, body: &Value) -> Result<Value, ApiError> {
        valid_index(name)?;
        let mapping = Mapping::parse(body)?;
        let schema = mapping.schema()?;
        let mut indexes = self
            .indexes
            .lock()
            .map_err(|_| ApiError::new(500, "internal_exception", "node state poisoned"))?;
        if indexes.contains_key(name) {
            return Err(hsearch_index::Error::Exists.into());
        }
        let index = Index::create(Arc::clone(&self.store), name, schema, &self.scratch(name))?;
        hsearch_index::put_mapping(&*self.store, name, &mapping.encode())?;
        indexes.insert(name.to_string(), Arc::new(Entry { index, mapping }));
        Ok(json!({"acknowledged": true, "shards_acknowledged": true, "index": name}))
    }

    /// `POST /_bulk` and `POST /{index}/_bulk`.
    pub fn bulk(
        &self,
        body: impl std::io::Read,
        default_index: Option<&str>,
        wait: Wait,
    ) -> Result<Value, ApiError> {
        let started = Instant::now();
        let items = bulk::parse(body, default_index)?;
        let mut responses: Vec<Option<Value>> = vec![None; items.len()];
        let mut errors = false;
        let mut i = 0;
        while i < items.len() {
            // A run of consecutive valid items for one index is one write.
            let name = &items[i].index;
            let entry = valid_index(name).and_then(|()| self.entry(name));
            let mut ops: Vec<Op> = Vec::new();
            let mut slots: Vec<usize> = Vec::new();
            let mut j = i;
            while j < items.len() && items[j].index == *name && ops.len() < CHUNK {
                let item = &items[j];
                let verdict = match (&entry, &item.work) {
                    (Err(e), _) | (_, Err(e)) => Err(e.clone()),
                    (Ok(entry), Ok((Action::Delete, _))) => {
                        let _ = entry;
                        Ok(Op::Delete {
                            id: item.id.clone(),
                        })
                    }
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
                        responses[j] = Some(item_error(&item.action, name, &item.id, &error));
                    }
                }
                j += 1;
            }
            if let (Ok(entry), false) = (&entry, ops.is_empty()) {
                match entry.index.bulk(ops, wait) {
                    Ok(results) => {
                        for (slot, result) in slots.iter().zip(results) {
                            responses[*slot] =
                                Some(item_result(&items[*slot].action, name, &result));
                        }
                    }
                    Err(error) => {
                        errors = true;
                        let error = ApiError::from(error);
                        for slot in &slots {
                            responses[*slot] = Some(item_error(
                                &items[*slot].action,
                                name,
                                &items[*slot].id,
                                &error,
                            ));
                        }
                    }
                }
            }
            i = j;
        }
        Ok(json!({
            "took": started.elapsed().as_millis() as u64,
            "errors": errors,
            "items": responses.into_iter().flatten().collect::<Vec<_>>(),
        }))
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

/// Build the write path's operation from a source object, in schema order.
fn document(mapping: &Mapping, id: &str, source: &Value) -> Result<Op, ApiError> {
    let object = source.as_object().ok_or_else(|| {
        ApiError::new(
            400,
            "mapper_parsing_exception",
            "the source must be a JSON object",
        )
    })?;
    if let Some(extra) = object.keys().find(|k| mapping.kind(k).is_none()) {
        return Err(ApiError::new(
            400,
            "strict_dynamic_mapping_exception",
            format!("unknown field [{extra}]; this node has no dynamic mapping"),
        ));
    }
    let mut fields = Vec::with_capacity(mapping.fields.len());
    for (name, kind) in &mapping.fields {
        let value = object.get(name).ok_or_else(|| {
            ApiError::new(
                400,
                "mapper_parsing_exception",
                format!("field [{name}] is required: every mapped field must be present"),
            )
        })?;
        fields.push(typed(*kind, value).map_err(|why| {
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
