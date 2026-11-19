//! Mappings are stored once by content hash and shared by every index that
//! declares them (PR-43), through the node's public API over a counting store.
mod support;

#[path = "support/counting.rs"]
mod counting;

use std::collections::BTreeMap;
use std::sync::Arc;

use counting::Counting;
use hsearch_node::{ApiError, Mapping, Node};
use serde_json::{Value, json};
use support::Scratch;

const SHARED: &str = "v1/local/mappings/";

/// The mapping body for variant `n`: a fixed 14-field shape whose first field
/// name differs, so each variant is a distinct mapping.
fn mapping(variant: usize, reversed: bool) -> String {
    let mut fields: Vec<String> = (0..14)
        .map(|i| {
            let name = if i == 0 {
                format!("variant{variant}")
            } else {
                format!("field{i}")
            };
            format!("\"{name}\":{{\"type\":\"keyword\"}}")
        })
        .collect();
    if reversed {
        fields.reverse();
    }
    format!(
        "{{\"mappings\":{{\"properties\":{{{}}}}}}}",
        fields.join(",")
    )
}

fn create(node: &Node<Counting>, name: &str, body: &str) -> Result<Value, ApiError> {
    node.create(name, &serde_json::from_str(body).unwrap())
}

#[test]
fn a_thousand_indexes_from_a_hundred_mappings_store_a_hundred_mapping_objects() {
    let (store, dir) = (Counting::new(), Scratch::new("share"));
    let node = Node::new(Arc::clone(&store), dir.0.clone());
    store.take();
    for i in 0..1000 {
        // Half the indexes spell their mapping with the keys in reverse order.
        create(&node, &format!("idx{i}"), &mapping(i % 100, i % 2 == 1)).unwrap();
    }
    let log = store.take();
    let mut objects: BTreeMap<&str, usize> = BTreeMap::new();
    for (kind, key) in &log {
        if *kind == "create" && key.starts_with(SHARED) {
            *objects.entry(key).or_default() += 1;
        }
    }
    assert_eq!(
        objects.len(),
        100,
        "one stored mapping object per distinct mapping"
    );
    assert!(
        objects.values().all(|n| *n == 1),
        "no mapping object is written twice"
    );
    // Past the first index with a mapping, a create costs no request for it.
    let shared_requests = log
        .iter()
        .filter(|(_, key)| key.starts_with(SHARED))
        .count();
    assert_eq!(
        shared_requests, 100,
        "{shared_requests} requests touched shared mappings"
    );
    println!(
        "mappings: 1000 indexes, 100 distinct mappings: {} mapping objects, {} requests on them, {} requests in all",
        objects.len(),
        shared_requests,
        log.len()
    );
}

#[test]
fn a_second_node_verifies_an_existing_object_and_writes_no_new_one() {
    let (store, dir) = (Counting::new(), Scratch::new("second"));
    let first = Node::new(Arc::clone(&store), dir.0.clone());
    create(&first, "one", &mapping(1, false)).unwrap();
    store.take();
    // A cold node has nothing cached: it tries the create, finds the object held
    // by the same bytes, and moves on. One object still.
    let dir2 = Scratch::new("second2");
    let second = Node::new(Arc::clone(&store), dir2.0.clone());
    create(&second, "two", &mapping(1, true)).unwrap();
    let log = store.take();
    assert!(
        log.iter()
            .any(|(k, key)| *k == "create" && key.starts_with(SHARED))
    );
    let held = log
        .iter()
        .filter(|(k, key)| *k == "get" && key.starts_with(SHARED))
        .count();
    assert_eq!(held, 1, "the held object is read once to compare it");
    // Opening either index reads the same single object back.
    let third = Node::new(Arc::clone(&store), Scratch::new("second3").0.clone());
    store.take();
    assert!(third.search("one", &json!({"size": 0})).is_ok());
    assert!(third.search("two", &json!({"size": 0})).is_ok());
    let log = store.take();
    let reads = log
        .iter()
        .filter(|(k, key)| *k == "get" && key.starts_with(SHARED))
        .count();
    assert_eq!(
        reads, 1,
        "two indexes with one mapping fetch it once per node"
    );
}

#[test]
fn indexes_never_resolve_another_indexes_mapping() {
    let (store, dir) = (Counting::new(), Scratch::new("isolate"));
    let node = Node::new(Arc::clone(&store), dir.0.clone());
    create(&node, "alpha", &mapping(1, false)).unwrap();
    create(&node, "beta", &mapping(2, false)).unwrap();
    // Each index's own reference resolves to its own mapping, and only that one.
    let alpha = hsearch_index::get_mapping(&*store, "alpha").unwrap();
    let beta = hsearch_index::get_mapping(&*store, "beta").unwrap();
    assert_ne!(alpha, beta);
    let wanted = Mapping::parse(&serde_json::from_str(&mapping(2, false)).unwrap()).unwrap();
    assert_eq!(Mapping::parse_reference(&beta), Some(wanted.hash()));
    assert!(
        node.search("beta", &json!({"query": {"term": {"variant2": "x"}}}))
            .is_ok()
    );
    // alpha has no `variant2` field: beta's mapping does not leak into it.
    let leak = node.search("alpha", &json!({"query": {"term": {"variant2": "x"}}}));
    assert!(leak.is_err(), "alpha must not see beta's field");
    // An index that does not exist reaches nothing, not even a shared object.
    store.take();
    assert!(node.search("gamma", &json!({})).is_err());
    assert!(store.take().iter().all(|(_, key)| !key.starts_with(SHARED)));
    // The shared object's key is the hash: knowing an index name gives no way to
    // name another index's object. (Per-principal reads are M05's; this node reads
    // a shared object only through the index's own reference.)
    assert!(alpha.len() < 100 && !String::from_utf8_lossy(&alpha).contains("variant"));
}

#[test]
fn a_hash_collision_or_a_tampered_object_is_refused_not_served() {
    let (store, dir) = (Counting::new(), Scratch::new("collide"));
    let node = Node::new(Arc::clone(&store), dir.0.clone());
    let parse =
        |variant| Mapping::parse(&serde_json::from_str(&mapping(variant, false)).unwrap()).unwrap();
    let (wanted, other) = (parse(5), parse(6));
    // Plant another mapping's object under the key this mapping's hash names.
    let key = format!("{SHARED}{}", hex(&wanted.hash()));
    hsearch_index::put_shared(&*store, &key, &other.to_object().unwrap()).unwrap();
    // Creating an index with the wanted mapping meets different bytes under its
    // hash: a collision, refused, and no index is left half-made and servable.
    let refused = create(&node, "victim", &mapping(5, false)).unwrap_err();
    assert_eq!((refused.status, refused.kind), (500, "corrupt_exception"));
    assert!(node.search("victim", &json!({})).is_err());
    // An index whose stored reference names that hash is refused on open: the
    // bytes behind the hash do not hash to it, so nothing is served.
    let schema = wanted.schema().unwrap();
    hsearch_index::Index::create(Arc::clone(&store), "liar", schema, &dir.0.join("liar")).unwrap();
    hsearch_index::put_mapping(&*store, "liar", &Mapping::reference(&wanted.hash())).unwrap();
    let fresh = Node::new(Arc::clone(&store), Scratch::new("collide2").0.clone());
    let error = fresh.search("liar", &json!({})).unwrap_err();
    assert_eq!((error.status, error.kind), (500, "corrupt_exception"));
    // A reference to an object that is not there is refused the same way.
    hsearch_index::Index::create(
        Arc::clone(&store),
        "dangling",
        parse(9).schema().unwrap(),
        &dir.0.join("dangling"),
    )
    .unwrap();
    hsearch_index::put_mapping(&*store, "dangling", &Mapping::reference(&parse(9).hash())).unwrap();
    let error = fresh.search("dangling", &json!({})).unwrap_err();
    assert_eq!((error.status, error.kind), (500, "corrupt_exception"));
}

#[test]
fn a_mapping_an_earlier_node_wrote_inline_still_opens() {
    let (store, dir) = (Counting::new(), Scratch::new("legacy"));
    let body: Value = serde_json::from_str(&mapping(3, false)).unwrap();
    let parsed = Mapping::parse(&body).unwrap();
    // The M20 layout: the index root, and the mapping bytes inline at `{index}/mapping`.
    let schema = parsed.schema().unwrap();
    let scratch = dir.0.join("old");
    hsearch_index::Index::create(Arc::clone(&store), "old", schema, &scratch).unwrap();
    hsearch_index::put_mapping(&*store, "old", &parsed.encode()).unwrap();
    let node = Node::new(Arc::clone(&store), dir.0.clone());
    assert!(node.search("old", &json!({"size": 0})).is_ok());
}

fn hex(hash: &[u8; 32]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
