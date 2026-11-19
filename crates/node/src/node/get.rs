//! Real-time get and multi-get (M21.05, PR-49 owner side).
//!
//! A read is answered by the index's owner from its own state, so it returns the
//! latest acknowledged write whether or not a refresh has made it searchable
//! (`realtime`, the default, as in OpenSearch); `realtime=false` reads the
//! searchable snapshot. No read costs an object-store request. A multi-get is
//! grouped by index, and an index is one shard on this node, so what it costs
//! follows the shards it touches and not the documents it asks for.
//!
//! Custom routing (`routing`, `_routing`) is accepted and has no effect: every
//! document of an index lives on the one shard this node owns, so any routing
//! value reaches it. Forwarding to another node's shard and the authority
//! barrier are M07.12.
use super::*;

/// Most documents one multi-get may ask for: bounds the request and the response.
pub(crate) const MAX_MGET_DOCS: usize = 1000;

fn doc_json(name: &str, id: &str, hit: Option<hsearch_engine::Hit>) -> (u16, Value) {
    match hit {
        None => (404, json!({"_index": name, "_id": id, "found": false})),
        Some(hit) => {
            let mut source = Map::new();
            for (field, value) in &hit.fields {
                source.insert(field.clone(), json_value(value));
            }
            (
                200,
                json!({"_index": name, "_id": id, "_version": hit.version,
                       "found": true, "_source": source}),
            )
        }
    }
}

impl<S: MetadataStore + 'static> Node<S> {
    /// `GET /{index}/_doc/{id}`: the latest acknowledged write.
    pub fn get(&self, name: &str, id: &str) -> Result<(u16, Value), ApiError> {
        self.get_with(name, id, true)
    }

    /// `GET /{index}/_doc/{id}?realtime=`: with `realtime` false, the searchable
    /// view only.
    pub fn get_with(&self, name: &str, id: &str, realtime: bool) -> Result<(u16, Value), ApiError> {
        check_id(id)?;
        let entry = self.entry(name)?;
        // The mapping first, then the read: a read is never older than the mapping
        // it is projected with, because an index evolves before its mapping does.
        let mapping = entry.mapping();
        let names = mapping.names();
        let mut found = entry.index.get_many(&[id], &names, realtime)?;
        Ok(doc_json(name, id, found.pop().flatten()))
    }

    /// `GET|POST /_mget` and `/{index}/_mget`: `{"ids": [...]}` (needs the index
    /// in the path) or `{"docs": [{"_index": .., "_id": ..}]}`. One answer per
    /// requested document, in order; a document that cannot be read (no such
    /// index, a bad id) answers with its own error and does not fail the others.
    /// Each index is read once for all of its documents.
    pub fn mget(
        &self,
        default: Option<&str>,
        body: &Value,
        realtime: bool,
    ) -> Result<(u16, Value), ApiError> {
        let wanted = parse_mget(default, body)?;
        let mut answers: Vec<Option<Value>> = vec![None; wanted.len()];
        // Group by index, keeping the order each was first named in.
        let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
        for (i, (index, id)) in wanted.iter().enumerate() {
            let Some(index) = index else {
                answers[i] = Some(item_failure(
                    "",
                    id,
                    &ApiError::new(
                        400,
                        "action_request_validation_exception",
                        "an index is required: name it in the path or in each document",
                    ),
                ));
                continue;
            };
            match groups.iter_mut().find(|(g, _)| g == index) {
                Some((_, members)) => members.push(i),
                None => groups.push((index, vec![i])),
            }
        }
        for (index, members) in groups {
            let ids: Vec<&str> = members.iter().map(|&i| wanted[i].1.as_str()).collect();
            match self.read_group(index, &ids, realtime) {
                Ok(hits) => {
                    for (&i, hit) in members.iter().zip(hits) {
                        answers[i] = Some(doc_json(index, &wanted[i].1, hit).1);
                    }
                }
                Err(error) => {
                    for &i in &members {
                        answers[i] = Some(item_failure(index, &wanted[i].1, &error));
                    }
                }
            }
        }
        Ok((
            200,
            json!({"docs": answers.into_iter().flatten().collect::<Vec<_>>()}),
        ))
    }

    fn read_group(
        &self,
        index: &str,
        ids: &[&str],
        realtime: bool,
    ) -> Result<Vec<Option<hsearch_engine::Hit>>, ApiError> {
        for id in ids {
            check_id(id)?;
        }
        let entry = self.entry(index)?;
        let mapping = entry.mapping();
        let names = mapping.names();
        Ok(entry.index.get_many(ids, &names, realtime)?)
    }
}

fn item_failure(index: &str, id: &str, error: &ApiError) -> Value {
    json!({"_index": index, "_id": id, "error": error.object()})
}

/// The documents a multi-get names, as (index, id), bounded and checked for shape.
fn parse_mget(
    default: Option<&str>,
    body: &Value,
) -> Result<Vec<(Option<String>, String)>, ApiError> {
    let object = body
        .as_object()
        .ok_or_else(|| ApiError::bad("the body must be a JSON object"))?;
    if let Some(key) = object.keys().find(|k| *k != "ids" && *k != "docs") {
        return Err(ApiError::bad(format!("unrecognized field [{key}]")));
    }
    let text = |value: &Value, what: &str| -> Result<String, ApiError> {
        value
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| ApiError::bad(format!("{what} must be a string")))
    };
    let mut wanted = Vec::new();
    match (object.get("ids"), object.get("docs")) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad("send either [ids] or [docs], not both"));
        }
        (None, None) => return Err(ApiError::bad("[ids] or [docs] is required")),
        (Some(ids), None) => {
            let ids = ids
                .as_array()
                .ok_or_else(|| ApiError::bad("[ids] must be an array"))?;
            if ids.len() > MAX_MGET_DOCS {
                return Err(too_many());
            }
            for id in ids {
                wanted.push((default.map(str::to_string), text(id, "an id")?));
            }
        }
        (None, Some(docs)) => {
            let docs = docs
                .as_array()
                .ok_or_else(|| ApiError::bad("[docs] must be an array"))?;
            if docs.len() > MAX_MGET_DOCS {
                return Err(too_many());
            }
            for doc in docs {
                let doc = doc
                    .as_object()
                    .ok_or_else(|| ApiError::bad("each document must be an object"))?;
                if let Some(key) = doc
                    .keys()
                    .find(|k| !["_index", "_id", "_routing", "routing"].contains(&k.as_str()))
                {
                    return Err(ApiError::bad(format!("unrecognized field [{key}]")));
                }
                let id = text(
                    doc.get("_id")
                        .ok_or_else(|| ApiError::bad("each document needs an [_id]"))?,
                    "[_id]",
                )?;
                let index = match doc.get("_index") {
                    Some(v) => Some(text(v, "[_index]")?),
                    None => default.map(str::to_string),
                };
                for routing in ["_routing", "routing"] {
                    if let Some(v) = doc.get(routing) {
                        text(v, routing)?;
                    }
                }
                wanted.push((index, id));
            }
        }
    }
    Ok(wanted)
}

fn too_many() -> ApiError {
    ApiError::bad(format!(
        "a multi-get asks for at most {MAX_MGET_DOCS} documents"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(default: Option<&str>, body: Value) -> String {
        parse_mget(default, &body).unwrap_err().reason
    }

    #[test]
    fn a_multi_get_body_is_checked_for_shape_and_size() {
        assert!(refused(None, json!([])).contains("JSON object"));
        assert!(refused(None, json!({"ids": ["a"], "x": 1})).contains("unrecognized field [x]"));
        assert!(refused(None, json!({})).contains("required"));
        assert!(refused(None, json!({"ids": "a"})).contains("array"));
        assert!(refused(None, json!({"docs": {}})).contains("array"));
        assert!(refused(None, json!({"docs": [1]})).contains("object"));
        assert!(
            refused(None, json!({"docs": [{"_id": "a", "_source": false}]})).contains("_source")
        );
        assert!(refused(None, json!({"docs": [{"_index": "i"}]})).contains("_id"));
        assert!(refused(None, json!({"docs": [{"_id": 1}]})).contains("string"));
        assert!(refused(None, json!({"docs": [{"_id": "a", "_routing": 1}]})).contains("string"));
        let many: Vec<Value> = (0..=MAX_MGET_DOCS).map(|i| json!(i.to_string())).collect();
        assert!(refused(Some("i"), json!({"ids": many.clone()})).contains("at most"));
        let docs: Vec<Value> = many.iter().map(|id| json!({"_id": id})).collect();
        assert!(refused(Some("i"), json!({"docs": docs})).contains("at most"));
        let ok = parse_mget(Some("i"), &json!({"ids": many[..MAX_MGET_DOCS]})).unwrap();
        assert_eq!(ok.len(), MAX_MGET_DOCS);
    }

    #[test]
    fn a_document_takes_its_own_index_or_the_default() {
        let got = parse_mget(
            Some("d"),
            &json!({"docs": [{"_id": "a"}, {"_id": "b", "_index": "x", "routing": "r"}]}),
        )
        .unwrap();
        assert_eq!(
            got,
            vec![
                (Some("d".to_string()), "a".to_string()),
                (Some("x".to_string()), "b".to_string())
            ]
        );
    }
}
