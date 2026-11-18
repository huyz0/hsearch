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
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "pq"}}),
            "must be [hnsw] or [ivf]",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "hnsw", "engine": "faiss"}}),
            "[method.engine] must be one of",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "ivf", "parameters": {"nlist": 128}}}),
            "unsupported method parameter [nlist]: the cluster count follows",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "ivf", "parameters": {"nprobes": 0}}}),
            "[nprobes] must be 1 to 65536",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "ivf", "parameters": {"nprobes": 70000}}}),
            "[nprobes] must be 1 to 65536",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "ivf", "parameters": {"encoder": {"name": "pq"}}}}),
            "there is no flat or pq encoder",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "ivf", "parameters": {"m": 8}}}),
            "unsupported method parameter [m]: it has no counterpart",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "ivf", "parameters": {"target_cluster_size": 8}}}),
            "[target_cluster_size] must be 16 to 65536",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "ivf", "parameters": {"rerank_encoding": "f64"}}}),
            "unsupported rerank_encoding",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "ivf"}, "quantizer": "scalar8"}),
            "an ivf field always keeps codes",
        ),
        (
            json!({"type": "knn_vector", "dimension": 4, "method": {"name": "ivf"}, "quantizer": "none"}),
            "an ivf field always keeps codes",
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

fn ivf_mapping() -> Value {
    json!({"mappings": {"properties": {
        "tag": {"type": "keyword"},
        "emb": {"type": "knn_vector", "dimension": 4, "space_type": "l2",
                "method": {"name": "ivf", "engine": "faiss",
                           "parameters": {"target_cluster_size": 16, "nprobes": 1,
                                          "rerank_encoding": "f16"}},
                "quantizer": "osq4"}
    }}})
}

fn ivf_knn(node: &Node<MemoryStore>, parameters: Value, filter: Option<Value>) -> Vec<String> {
    let mut spec =
        json!({"vector": [10.2, 0.0, 0.5, 1.0], "k": 3, "method_parameters": parameters});
    if let Some(filter) = filter {
        spec["filter"] = filter;
    }
    ids(&node
        .search(
            "v",
            &json!({"query": {"knn": {"emb": spec}}, "size": 3, "_source": false}),
        )
        .unwrap())
}

#[test]
fn an_ivf_field_is_mapped_searched_with_nprobes_and_survives_a_restart() {
    let scratch = Scratch::new("vectors-ivf");
    let store = store();
    let node = Node::with_limits(Arc::clone(&store), scratch.0.clone(), Limits::default()).unwrap();
    node.create("v", &ivf_mapping()).unwrap();
    load(&node);

    // The forty documents lie on a line in a few clusters, and 14.5 sits at a
    // cluster boundary: the mapping's default of one probed cluster misses a
    // true neighbour that probing every cluster finds, so the default and the
    // per-query override are each shown to be honoured.
    let at_boundary = |parameters: Value| {
        let spec = json!({"vector": [14.5, 0.0, 0.5, 0.0], "k": 3,
            "method_parameters": parameters});
        ids(&node
            .search(
                "v",
                &json!({"query": {"knn": {"emb": spec}}, "size": 3, "_source": false}),
            )
            .unwrap())
    };
    assert_eq!(at_boundary(json!({})), vec!["d15", "d16", "d17"]);
    assert_eq!(
        at_boundary(json!({"nprobes": 1000})),
        vec!["d15", "d13", "d16"]
    );

    let all = json!({"nprobes": 1000});
    assert_eq!(ivf_knn(&node, all.clone(), None), vec!["d10", "d11", "d9"]);
    let even = json!({"term": {"tag": "even"}});
    assert_eq!(
        ivf_knn(&node, all.clone(), Some(even.clone())),
        vec!["d10", "d12", "d8"]
    );
    assert_eq!(
        ivf_knn(&node, json!({"nprobes": 4, "ef_search": 10}), None).len(),
        3
    );

    // The index and its mapping survive a restart.
    drop(node);
    let node = Node::with_limits(store, scratch.0.clone(), Limits::default()).unwrap();
    assert_eq!(ivf_knn(&node, all.clone(), None), vec!["d10", "d11", "d9"]);
    assert_eq!(ivf_knn(&node, all, Some(even)), vec!["d10", "d12", "d8"]);
}

#[test]
fn an_on_disk_hnsw_field_is_mapped_searched_and_survives_a_restart() {
    let scratch = Scratch::new("vectors-disk");
    let store = store();
    let node = Node::with_limits(Arc::clone(&store), scratch.0.clone(), Limits::default()).unwrap();
    node.create(
        "v",
        &json!({"mappings": {"properties": {
            "tag": {"type": "keyword"},
            "emb": {"type": "knn_vector", "dimension": 4, "space_type": "l2",
                    "mode": "on_disk", "quantizer": "osq4"}
        }}}),
    )
    .unwrap();
    load(&node);
    let search = |node: &Node<MemoryStore>, parameters: Value, filter: Option<Value>| {
        let mut spec =
            json!({"vector": [10.2, 0.0, 0.5, 1.0], "k": 3, "method_parameters": parameters});
        if let Some(filter) = filter {
            spec["filter"] = filter;
        }
        ids(&node
            .search(
                "v",
                &json!({"query": {"knn": {"emb": spec}}, "size": 3, "_source": false}),
            )
            .unwrap())
    };
    let even = json!({"term": {"tag": "even"}});
    assert_eq!(search(&node, json!({}), None), vec!["d10", "d11", "d9"]);
    assert_eq!(
        search(&node, json!({"ef_search": 40}), Some(even.clone())),
        vec!["d10", "d12", "d8"]
    );
    // A disk field is an hnsw graph: it takes ef_search and refuses nprobes.
    let error = node
        .search(
            "v",
            &json!({"query": {"knn": {"emb": {"vector": [1,2,3,4], "k": 1,
                "method_parameters": {"nprobes": 3}}}}}),
        )
        .unwrap_err();
    assert!(error.reason.contains("nprobes applies to an ivf field"));
    drop(node);
    let node = Node::with_limits(store, scratch.0.clone(), Limits::default()).unwrap();
    assert_eq!(search(&node, json!({}), None), vec!["d10", "d11", "d9"]);
    assert_eq!(
        search(&node, json!({}), Some(even)),
        vec!["d10", "d12", "d8"]
    );
}

#[test]
fn an_ivf_field_without_nprobes_is_accepted_and_searches() {
    let scratch = Scratch::new("vectors-ivf-auto");
    let node = Node::with_limits(store(), scratch.0.clone(), Limits::default()).unwrap();
    node.create(
        "v",
        &json!({"mappings": {"properties": {
            "tag": {"type": "keyword"},
            "emb": {"type": "knn_vector", "dimension": 4, "space_type": "l2",
                    "method": {"name": "ivf", "engine": "faiss",
                               "parameters": {"target_cluster_size": 16}}}
        }}}),
    )
    .unwrap();
    load(&node);
    assert_eq!(ivf_knn(&node, json!({}), None), vec!["d10", "d11", "d9"]);
    // A query may still name its own depth.
    assert_eq!(
        ivf_knn(&node, json!({"nprobes": 1000}), None),
        vec!["d10", "d11", "d9"]
    );
}

#[test]
fn nprobes_on_an_hnsw_field_and_a_bad_nprobes_are_refused_by_name() {
    let scratch = Scratch::new("vectors-nprobes");
    let node = Node::with_limits(store(), scratch.0.clone(), Limits::default()).unwrap();
    node.create("v", &mapping()).unwrap();
    node.create("w", &ivf_mapping()).unwrap();
    let refused = |index: &str, parameters: Value| {
        node.search(
            index,
            &json!({"query": {"knn": {"emb": {"vector": [1,2,3,4], "k": 1,
                "method_parameters": parameters}}}}),
        )
        .unwrap_err()
        .reason
    };
    assert!(refused("v", json!({"nprobes": 3})).contains("nprobes applies to an ivf field"));
    assert!(refused("w", json!({"nprobes": 0})).contains("[nprobes] must be"));
    assert!(refused("w", json!({"nprobes": 70000})).contains("[nprobes] must be"));
    assert!(refused("w", json!({"nprobe": 3})).contains("unsupported method parameter [nprobe]"));
}
