//! Vector fields through the node: a `knn_vector` mapping, bulk ingest of
//! vectors, the `knn` query with a filter, and a restart that rebuilds the
//! index from the object store.
mod support;

use std::sync::Arc;

use hsearch_index::Wait;
use hsearch_node::{Limits, Node};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use serde_json::{Value, json};
use support::Scratch;

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

fn mapping() -> Value {
    json!({"mappings": {"properties": {
        "tag": {"type": "keyword"},
        "emb": {"type": "knn_vector", "dimension": 4, "space_type": "l2",
                "method": {"name": "hnsw", "parameters": {"m": 8, "ef_construction": 64}},
                "quantizer": "scalar8"}
    }}})
}

fn bulk(node: &Node<MemoryStore>, text: &str) -> Value {
    let (status, response) = node
        .bulk(
            text.as_bytes(),
            Some(text.len() as u64),
            Some("v"),
            Wait::Searchable,
        )
        .unwrap();
    assert_eq!(status, 200, "{response}");
    response
}

fn ids(response: &Value) -> Vec<String> {
    response["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap().to_string())
        .collect()
}

fn load(node: &Node<MemoryStore>) {
    let mut text = String::new();
    for i in 0..40 {
        let v = [i as f32, 0.0, 0.5, (i % 3) as f32];
        text.push_str(&format!(
            "{{\"index\":{{\"_id\":\"d{i}\"}}}}\n{{\"tag\":\"{}\",\"emb\":{:?}}}\n",
            if i % 2 == 0 { "even" } else { "odd" },
            v
        ));
    }
    let response = bulk(node, &text);
    assert_eq!(response["errors"], json!(false), "{response}");
}

fn knn(node: &Node<MemoryStore>, filter: Option<Value>) -> Vec<String> {
    let mut spec = json!({"vector": [10.2, 0.0, 0.5, 1.0], "k": 3});
    if let Some(filter) = filter {
        spec["filter"] = filter;
    }
    let response = node
        .search(
            "v",
            &json!({"query": {"knn": {"emb": spec}}, "size": 3, "_source": false}),
        )
        .unwrap();
    ids(&response)
}

#[test]
fn vectors_are_indexed_searched_filtered_and_survive_a_restart() {
    let scratch = Scratch::new("vectors");
    let store = store();
    let node = Node::with_limits(Arc::clone(&store), scratch.0.clone(), Limits::default()).unwrap();
    node.create("v", &mapping()).unwrap();
    load(&node);

    assert_eq!(knn(&node, None), vec!["d10", "d11", "d9"]);
    let even = json!({"term": {"tag": "even"}});
    assert_eq!(knn(&node, Some(even.clone())), vec!["d10", "d12", "d8"]);

    // The vector is indexed, not stored: a document's source omits it.
    let (status, got) = node.get("v", "d10").unwrap();
    assert_eq!(status, 200);
    assert_eq!(got["_source"], json!({"tag": "even"}));

    // A restart rebuilds the same index, mapping included, from the store.
    drop(node);
    let node = Node::with_limits(store, scratch.0.clone(), Limits::default()).unwrap();
    assert_eq!(knn(&node, None), vec!["d10", "d11", "d9"]);
    assert_eq!(knn(&node, Some(even)), vec!["d10", "d12", "d8"]);
}

#[test]
fn bad_vectors_mappings_and_queries_are_refused_by_name() {
    let scratch = Scratch::new("vectors-bad");
    let node = Node::with_limits(store(), scratch.0.clone(), Limits::default()).unwrap();
    for (body, why) in [
        (json!({"type": "knn_vector"}), "[dimension] is required"),
        (
            json!({"type": "knn_vector", "dimension": 0}),
            "out of range",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "engine": "faiss"}),
            "unsupported knn_vector parameter [engine]",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "space_type": "hamming"}),
            "unsupported space_type",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "quantizer": "pq"}),
            "unsupported quantizer",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "ivf"}}),
            "must be [hnsw]",
        ),
    ] {
        let error = node
            .create("bad", &json!({"mappings": {"properties": {"e": body}}}))
            .unwrap_err();
        assert!(error.reason.contains(why), "{why}: {}", error.reason);
    }

    node.create("v", &mapping()).unwrap();
    let wrong = |source: &str| {
        let text = format!("{{\"index\":{{\"_id\":\"x\"}}}}\n{source}\n");
        let response = bulk(&node, &text);
        assert_eq!(response["errors"], json!(true), "{source}");
        response["items"][0]["index"]["error"]["reason"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(wrong(r#"{"tag":"a","emb":[1,2,3]}"#).contains("3 components"));
    assert!(wrong(r#"{"tag":"a","emb":[1,2,3,"x"]}"#).contains("only numbers"));
    assert!(wrong(r#"{"tag":"a","emb":[1,2,3,1e39]}"#).contains("finite"));
    assert!(wrong(r#"{"tag":"a","emb":[1,2,[3],4]}"#).contains("nested"));
    assert!(wrong(r#"{"tag":"a","emb":"abcd"}"#).contains("does not match"));

    for (query, why) in [
        (
            json!({"knn": {"tag": {"vector": [1,2,3,4], "k": 1}}}),
            "not a knn_vector",
        ),
        (
            json!({"knn": {"emb": {"vector": [1,2,3], "k": 1}}}),
            "3 components",
        ),
        (json!({"knn": {"emb": {"vector": [1,2,3,4]}}}), "needs k"),
        (
            json!({"knn": {"emb": {"vector": [1,2,3,4], "k": 0}}}),
            "[k] must be",
        ),
        (
            json!({"knn": {"emb": {"vector": [1,2,3,4], "k": 1, "boost": 2}}}),
            "unsupported knn parameter",
        ),
        (
            json!({"knn": {"emb": {"vector": [1,2,3,4], "k": 1, "method_parameters": {"nprobes": 3}}}}),
            "unsupported method parameter",
        ),
    ] {
        let error = node.search("v", &json!({"query": query})).unwrap_err();
        assert!(error.reason.contains(why), "{why}: {}", error.reason);
    }
}
