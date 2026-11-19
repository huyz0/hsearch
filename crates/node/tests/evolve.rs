//! Online mapping evolution (M21.04, PR-44), through the node's public API: fields
//! are added to a live index as a new immutable mapping version, writes and
//! searches keep running, and a change that cannot be made in place is refused
//! with the mapping unchanged.
#[path = "support/counting.rs"]
mod counting;
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use counting::Counting;
use hsearch_index::Wait;
use hsearch_node::{ApiError, Limits, Mapping, MappingLimits, Node};
use serde_json::{Value, json};
use support::Scratch;

fn node_on(store: &Arc<Counting>, dir: &std::path::Path) -> Node<Counting> {
    Node::with_limits(Arc::clone(store), dir.to_path_buf(), Limits::default()).unwrap()
}

fn create(node: &Node<Counting>, dynamic: &str) {
    node.create(
        "ix",
        &json!({"mappings": {"dynamic": dynamic, "properties": {
            "title": {"type": "text"}, "n": {"type": "integer"}}}}),
    )
    .unwrap();
}

fn bulk(node: &Node<Counting>, sources: &[(String, Value)], wait: Wait) -> Value {
    let mut text = String::new();
    for (id, source) in sources {
        text.push_str(&format!("{{\"index\":{{\"_id\":\"{id}\"}}}}\n{source}\n"));
    }
    let (status, response) = node
        .bulk(text.as_bytes(), Some(text.len() as u64), Some("ix"), wait)
        .unwrap();
    assert_eq!(status, 200, "{response}");
    response
}

fn put(node: &Node<Counting>, id: &str, source: Value) -> Value {
    bulk(node, &[(id.to_string(), source)], Wait::Searchable)
}

fn total(node: &Node<Counting>, query: Value) -> u64 {
    node.search("ix", &json!({"size": 0, "query": query}))
        .unwrap()["hits"]["total"]["value"]
        .as_u64()
        .unwrap()
}

fn mapping_requests(store: &Counting) -> Vec<(&'static str, String)> {
    store
        .take()
        .into_iter()
        .filter(|(_, key)| key.ends_with("/mapping") || key.starts_with("v1/"))
        .collect()
}

#[test]
fn a_field_added_to_a_live_index_is_searchable_and_old_documents_stay_readable() {
    let (store, dir) = (Counting::new(), Scratch::new("add"));
    let node = node_on(&store, &dir.0);
    create(&node, "strict");
    put(&node, "old", json!({"title": "quick fox", "n": 1}));

    // Strict refuses the field until the mapping says it exists.
    let refused = put(&node, "new0", json!({"title": "a", "n": 2, "tag": "hot"}));
    assert_eq!(refused["errors"], json!(true));
    node.put_mapping(
        "ix",
        &json!({"properties": {"tag": {"type": "keyword"}, "rank": {"type": "integer"}}}),
    )
    .unwrap();

    put(
        &node,
        "new1",
        json!({"title": "lazy dog", "n": 3, "tag": "hot", "rank": 7}),
    );
    // A document written under the new version may leave an added field out.
    put(&node, "new2", json!({"title": "other", "n": 4}));

    assert_eq!(total(&node, json!({"term": {"tag": "hot"}})), 1);
    assert_eq!(total(&node, json!({"range": {"rank": {"gte": 5}}})), 1);
    assert_eq!(total(&node, json!({"match_all": {}})), 3);
    let (status, old) = node.get("ix", "old").unwrap();
    assert_eq!(status, 200);
    assert_eq!(old["_source"], json!({"n": 1, "title": "quick fox"}));
    let (_, fresh) = node.get("ix", "new1").unwrap();
    assert_eq!(fresh["_source"]["tag"], json!("hot"));
    assert_eq!(fresh["_source"]["rank"], json!(7));
    // Sorting on the added field across a segment that never had it still answers.
    let sorted = node
        .search("ix", &json!({"sort": [{"rank": "asc"}], "size": 10}))
        .unwrap();
    assert_eq!(sorted["hits"]["total"]["value"], json!(3));
}

#[test]
fn a_type_change_is_refused_and_the_mapping_is_unchanged() {
    let (store, dir) = (Counting::new(), Scratch::new("refuse"));
    let node = node_on(&store, &dir.0);
    create(&node, "strict");
    let before = hsearch_index::get_mapping(&*store, "ix").unwrap();
    store.take();
    for (body, fragment) in [
        (
            json!({"properties": {"title": {"type": "keyword"}}}),
            "cannot be changed from type [text] to [keyword]",
        ),
        (
            json!({"properties": {"n": {"type": "text"}, "extra": {"type": "keyword"}}}),
            "_reindex",
        ),
        (
            json!({"properties": {"v": {"type": "knn_vector", "dimension": 3}}}),
            "cannot be added to an existing index",
        ),
    ] {
        let error: ApiError = node.put_mapping("ix", &body).unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.reason.contains(fragment), "{}", error.reason);
    }
    assert!(
        mapping_requests(&store)
            .iter()
            .all(|(kind, _)| *kind != "create" && *kind != "cas"),
        "a refusal writes nothing"
    );
    assert_eq!(hsearch_index::get_mapping(&*store, "ix").unwrap(), before);
    // The refused field, in the same request as a refused one, was not half added.
    assert_eq!(
        put(&node, "d", json!({"title": "a", "n": 1, "extra": "x"}))["errors"],
        json!(true)
    );
}

#[test]
fn a_repeated_or_unchanged_put_mapping_is_acknowledged_and_costs_nothing() {
    let (store, dir) = (Counting::new(), Scratch::new("idem"));
    let node = node_on(&store, &dir.0);
    create(&node, "strict");
    let body = json!({"properties": {"tag": {"type": "keyword"}}});
    node.put_mapping("ix", &body).unwrap();
    let after = hsearch_index::get_mapping(&*store, "ix").unwrap();
    store.take();
    // The same field again, and an existing field redeclared as it is.
    node.put_mapping("ix", &body).unwrap();
    node.put_mapping("ix", &json!({"properties": {"title": {"type": "text"}}}))
        .unwrap();
    node.put_mapping("ix", &json!({"dynamic": "strict"}))
        .unwrap();
    assert_eq!(
        store.take().len(),
        0,
        "no request for a change that changes nothing"
    );
    assert_eq!(hsearch_index::get_mapping(&*store, "ix").unwrap(), after);
    // A body that says nothing, or something unsupported, is refused.
    for bad in [
        json!({}),
        json!([]),
        json!({"properties": []}),
        json!({"settings": {}}),
    ] {
        assert!(node.put_mapping("ix", &bad).is_err(), "{bad}");
    }
    assert_eq!(node.put_mapping("nope", &body).unwrap_err().status, 404);
}

#[test]
fn a_change_costs_the_same_requests_for_a_small_index_and_a_large_one() {
    let (store, dir) = (Counting::new(), Scratch::new("cost"));
    let node = node_on(&store, &dir.0);
    let mut counts = Vec::new();
    for (name, docs) in [("small", 5usize), ("large", 1500)] {
        node.create(
            name,
            &json!({"mappings": {"properties": {"title": {"type": "text"}}}}),
        )
        .unwrap();
        let mut text = String::new();
        for i in 0..docs {
            text.push_str(&format!(
                "{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"title\":\"doc {i}\"}}\n"
            ));
        }
        node.bulk(
            text.as_bytes(),
            Some(text.len() as u64),
            Some(name),
            Wait::Searchable,
        )
        .unwrap();
        // Two fresh fields each, named differently, so each change makes a mapping
        // object of its own rather than finding one this node already holds.
        store.take();
        let (a, b) = (format!("{name}_a"), format!("{name}_b"));
        node.put_mapping(
            name,
            &json!({"properties": {a: {"type": "keyword"}, b: {"type": "integer"}}}),
        )
        .unwrap();
        let log = store.take();
        assert!(log.iter().all(|(kind, _)| *kind != "list"), "no LIST");
        counts.push(
            log.iter()
                .fold(std::collections::BTreeMap::new(), |mut m, (k, _)| {
                    *m.entry(*k).or_insert(0usize) += 1;
                    m
                }),
        );
    }
    assert_eq!(
        counts[0], counts[1],
        "requests do not scale with documents: {counts:?}"
    );
    println!("put-mapping requests, 5 and 1,500 documents: {counts:?}");
}

#[test]
fn the_new_version_survives_a_restart_and_old_segments_replay() {
    let (store, dir) = (Counting::new(), Scratch::new("restart"));
    let first = node_on(&store, &dir.0.join("one"));
    create(&first, "strict");
    put(&first, "old", json!({"title": "kept", "n": 1}));
    first
        .put_mapping("ix", &json!({"properties": {"tag": {"type": "keyword"}}}))
        .unwrap();
    put(&first, "new", json!({"title": "added", "n": 2, "tag": "x"}));
    drop(first);

    // A node that knows nothing of the first one rebuilds from the store alone.
    let second = node_on(&store, &dir.0.join("two"));
    assert_eq!(total(&second, json!({"match_all": {}})), 2);
    assert_eq!(total(&second, json!({"term": {"tag": "x"}})), 1);
    let (_, old) = second.get("ix", "old").unwrap();
    assert_eq!(old["_source"], json!({"n": 1, "title": "kept"}));
    // The stored mapping is the evolved one, reachable by its version.
    let reference = hsearch_index::get_mapping(&*store, "ix").unwrap();
    let hash = Mapping::parse_reference(&reference).unwrap();
    let object =
        hsearch_index::get_shared(&*store, &format!("v1/local/mappings/{}", hex(&hash))).unwrap();
    let mapping = Mapping::from_object(&object, &hash).unwrap();
    assert!(
        String::from_utf8(mapping.encode())
            .unwrap()
            .contains("#evolution\t2\t1")
    );
    // And a field the stored mapping has cannot be redeclared as another type.
    assert!(
        second
            .put_mapping("ix", &json!({"properties": {"tag": {"type": "text"}}}))
            .is_err()
    );
}

fn hex(hash: &[u8; 32]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn a_field_added_while_writes_and_searches_run_loses_no_write() {
    let (store, dir) = (Counting::new(), Scratch::new("live"));
    let node = Arc::new(node_on(&store, &dir.0));
    // Dynamic true: writers that send the new field before anyone has declared it
    // add it themselves, and every other writer's documents are unaffected.
    create(&node, "strict");
    let stop = Arc::new(AtomicBool::new(false));

    let writers: Vec<_> = (0..4)
        .map(|w| {
            let node = Arc::clone(&node);
            std::thread::spawn(move || {
                let mut acknowledged = Vec::new();
                for i in 0..80 {
                    let id = format!("w{w}-{i}");
                    // Half the documents carry the new field. Before it exists, a
                    // strict index refuses those items; they are retried until the
                    // mapping change lands, so every document is eventually written.
                    let source = if i % 2 == 0 {
                        json!({"title": format!("t{i}"), "n": i})
                    } else {
                        json!({"title": format!("t{i}"), "n": i, "tag": "hot"})
                    };
                    // Bounded, so a mapping change that never lands fails the test
                    // instead of hanging it.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                    loop {
                        let r = bulk(&node, &[(id.clone(), source.clone())], Wait::Durable);
                        if r["errors"] == json!(false) {
                            acknowledged.push(id);
                            break;
                        }
                        assert!(std::time::Instant::now() < deadline, "{id}: {r}");
                        std::thread::yield_now();
                    }
                }
                acknowledged
            })
        })
        .collect();
    let searcher = {
        let (node, stop) = (Arc::clone(&node), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut searches = 0;
            while !stop.load(Ordering::Relaxed) {
                node.search("ix", &json!({"size": 1, "query": {"match_all": {}}}))
                    .unwrap();
                // Once the field exists it is searchable at once.
                let _ = node.search("ix", &json!({"size": 1, "query": {"term": {"tag": "hot"}}}));
                searches += 1;
            }
            searches
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(20));
    node.put_mapping("ix", &json!({"properties": {"tag": {"type": "keyword"}}}))
        .unwrap();
    let acknowledged: usize = writers.into_iter().map(|h| h.join().unwrap().len()).sum();
    stop.store(true, Ordering::Relaxed);
    let searches = searcher.join().unwrap();
    node.refresh("ix").unwrap();

    assert_eq!(acknowledged, 4 * 80);
    assert_eq!(
        total(&node, json!({"match_all": {}})) as usize,
        acknowledged
    );
    assert_eq!(total(&node, json!({"term": {"tag": "hot"}})), 4 * 40);
    println!("searches during the change: {searches}");
}

#[test]
fn dynamic_true_maps_new_fields_once_per_bulk() {
    let (store, dir) = (Counting::new(), Scratch::new("dyn"));
    let node = node_on(&store, &dir.0);
    create(&node, "true");
    store.take();
    // One bulk introducing two fields across a hundred documents: one mapping
    // change, not one per document.
    let sources: Vec<(String, Value)> = (0..100)
        .map(|i| {
            (
                format!("d{i}"),
                json!({"title": "x", "n": i, "color": "red", "count": i, "ok": true, "gone": null}),
            )
        })
        .collect();
    let response = bulk(&node, &sources, Wait::Searchable);
    assert_eq!(response["errors"], json!(false), "{response}");
    let changes = mapping_requests(&store)
        .iter()
        .filter(|(kind, key)| *kind == "cas" && key.ends_with("/mapping"))
        .count();
    assert_eq!(changes, 1, "one compare-exchange of the mapping");
    // Strings become text, whole numbers integer, booleans boolean; null maps nothing.
    assert_eq!(total(&node, json!({"match": {"color": "red"}})), 100);
    assert_eq!(total(&node, json!({"range": {"count": {"gte": 50}}})), 50);
    assert_eq!(total(&node, json!({"term": {"ok": true}})), 100);
    let (_, doc) = node.get("ix", "d1").unwrap();
    assert!(doc["_source"].get("gone").is_none());
    // A second bulk with the same fields changes nothing.
    store.take();
    let again: Vec<(String, Value)> =
        vec![("e".into(), json!({"title": "y", "n": 1, "color": "blue"}))];
    assert_eq!(
        bulk(&node, &again, Wait::Searchable)["errors"],
        json!(false)
    );
    assert!(
        mapping_requests(&store)
            .iter()
            .all(|(kind, _)| *kind != "cas")
    );
}

#[test]
fn dynamic_true_refuses_the_item_it_cannot_map_and_keeps_the_rest() {
    let (store, dir) = (Counting::new(), Scratch::new("dynbad"));
    let node = node_on(&store, &dir.0);
    create(&node, "true");
    let sources: Vec<(String, Value)> = vec![
        ("ok".into(), json!({"title": "a", "n": 1, "color": "red"})),
        // A type another document fixed first.
        ("clash".into(), json!({"title": "a", "n": 1, "color": 5})),
        ("float".into(), json!({"title": "a", "n": 1, "price": 1.5})),
        (
            "big".into(),
            json!({"title": "a", "n": 1, "huge": 5_000_000_000_i64}),
        ),
        ("list".into(), json!({"title": "a", "n": 1, "tags": ["a"]})),
        (
            "name".into(),
            json!({"title": "a", "n": 1, "bad name": "x"}),
        ),
        (
            "fine".into(),
            json!({"title": "a", "n": 1, "color": "blue"}),
        ),
    ];
    let response = bulk(&node, &sources, Wait::Searchable);
    let status = |i: usize| response["items"][i]["index"]["status"].as_u64().unwrap();
    assert_eq!(
        (0..7).map(status).collect::<Vec<_>>(),
        [201, 400, 400, 400, 400, 400, 201],
        "{response}"
    );
    for i in [1, 2, 3, 4, 5] {
        assert_eq!(
            response["items"][i]["index"]["error"]["type"],
            json!("mapper_parsing_exception"),
            "{response}"
        );
    }
    assert_eq!(total(&node, json!({"match_all": {}})), 2);
    // A refused item added none of its fields: price, huge, tags and "bad name" are
    // not in the mapping, so a strict put-mapping may still declare them differently.
    node.put_mapping("ix", &json!({"properties": {"price": {"type": "integer"}}}))
        .unwrap();
}

#[test]
fn dynamic_true_stops_at_the_field_limit_and_the_engine_bound_without_half_applying() {
    let (store, dir) = (Counting::new(), Scratch::new("limit"));
    let limits = MappingLimits {
        max_fields: 5,
        ..MappingLimits::default()
    };
    let node = node_on(&store, &dir.0).with_mapping_limits(limits).unwrap();
    create(&node, "true");
    // 2 declared + 3 more is the limit; the fourth field is refused with the limit's name.
    let response = bulk(
        &node,
        &[
            (
                "a".into(),
                json!({"title": "a", "n": 1, "f1": "x", "f2": "x", "f3": "x"}),
            ),
            ("b".into(), json!({"title": "a", "n": 1, "f4": "x"})),
        ],
        Wait::Searchable,
    );
    assert_eq!(
        response["items"][0]["index"]["status"],
        json!(201),
        "{response}"
    );
    assert!(
        response["items"][1]["index"]["error"]["reason"]
            .as_str()
            .unwrap()
            .contains("Limit of total fields [5]"),
        "{response}"
    );
    let error = node
        .put_mapping("ix", &json!({"properties": {"f5": {"type": "keyword"}}}))
        .unwrap_err();
    assert!(error.reason.contains("Limit of total fields [5]"));
    assert_eq!(total(&node, json!({"match_all": {}})), 1);

    // The engine's own bound (14 fields) sits under a configured limit above it.
    let node2 = node_on(&store, &dir.0.join("two"));
    node2
        .create(
            "wide",
            &json!({"mappings": {"dynamic": true, "properties": {"a": {"type": "keyword"}}}}),
        )
        .unwrap();
    let wide: serde_json::Map<String, Value> =
        (0..14).map(|i| (format!("f{i}"), json!("x"))).collect();
    let text = format!(
        "{{\"index\":{{\"_id\":\"1\"}}}}\n{}\n",
        Value::Object({
            let mut m = wide;
            m.insert("a".into(), json!("x"));
            m
        })
    );
    let (_, response) = node2
        .bulk(
            text.as_bytes(),
            Some(text.len() as u64),
            Some("wide"),
            Wait::Searchable,
        )
        .unwrap();
    assert_eq!(response["errors"], json!(true), "{response}");
    assert!(
        response["items"][0]["index"]["error"]["reason"]
            .as_str()
            .unwrap()
            .contains("at most 14 fields"),
        "{response}"
    );
}

#[test]
fn the_dynamic_setting_can_be_changed_online() {
    let (store, dir) = (Counting::new(), Scratch::new("setting"));
    let node = node_on(&store, &dir.0);
    create(&node, "strict");
    assert_eq!(
        put(&node, "a", json!({"title": "a", "n": 1, "extra": "x"}))["errors"],
        json!(true)
    );
    node.put_mapping("ix", &json!({"dynamic": true})).unwrap();
    assert_eq!(
        put(&node, "a", json!({"title": "a", "n": 1, "extra": "x"}))["errors"],
        json!(false)
    );
    node.put_mapping("ix", &json!({"dynamic": false})).unwrap();
    let r = put(&node, "b", json!({"title": "a", "n": 1, "ignored": "x"}));
    assert_eq!(r["errors"], json!(false));
    assert!(
        node.get("ix", "b").unwrap().1["_source"]
            .get("ignored")
            .is_none()
    );
    assert!(
        node.put_mapping("ix", &json!({"dynamic": "maybe"}))
            .is_err()
    );
}

#[test]
fn a_mapping_changed_by_another_writer_is_not_overwritten() {
    let (store, dir) = (Counting::new(), Scratch::new("race"));
    let first = node_on(&store, &dir.0.join("one"));
    create(&first, "strict");
    // A second node adds one field behind the first node's back.
    let second = node_on(&store, &dir.0.join("two"));
    second
        .put_mapping("ix", &json!({"properties": {"b": {"type": "keyword"}}}))
        .unwrap();
    // The first still holds the old version: its change is refused, not applied over.
    let error = first
        .put_mapping("ix", &json!({"properties": {"a": {"type": "keyword"}}}))
        .unwrap_err();
    assert_eq!(error.status, 409, "{error:?}");
    // It reopens under what the store holds and can then add its field on top of it.
    first
        .put_mapping("ix", &json!({"properties": {"a": {"type": "keyword"}}}))
        .unwrap();
    let reference = hsearch_index::get_mapping(&*store, "ix").unwrap();
    let hash = Mapping::parse_reference(&reference).unwrap();
    let object =
        hsearch_index::get_shared(&*store, &format!("v1/local/mappings/{}", hex(&hash))).unwrap();
    let text = String::from_utf8(Mapping::from_object(&object, &hash).unwrap().encode()).unwrap();
    assert!(text.contains("#evolution\t3\t2"), "{text}");
    assert!(text.contains("\nb\tkeyword\na\tkeyword\n"), "{text}");
}
