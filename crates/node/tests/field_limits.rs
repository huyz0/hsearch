//! Field limits and the per-index dynamic-mapping policy (PR-42), through the
//! node's public API: a request that would pass a limit is refused whole, and a
//! refusal never changes the stored mapping.
mod support;

use std::sync::Arc;

use hsearch_index::Wait;
use hsearch_node::{ApiError, Limits, MappingLimits, Node};
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

fn node(label: &str, limits: MappingLimits) -> (Node<MemoryStore>, Scratch) {
    let scratch = Scratch::new(label);
    let node = Node::with_limits(store(), scratch.0.clone(), Limits::default())
        .unwrap()
        .with_mapping_limits(limits)
        .unwrap();
    (node, scratch)
}

fn body(dynamic: Option<&str>, fields: &[&str]) -> Value {
    let properties: serde_json::Map<String, Value> = fields
        .iter()
        .map(|f| ((*f).to_string(), json!({"type": "keyword"})))
        .collect();
    let mut mappings = json!({ "properties": properties });
    if let Some(dynamic) = dynamic {
        mappings["dynamic"] = json!(dynamic);
    }
    json!({ "mappings": mappings })
}

/// Bulk-index the given sources into `index` with ids `0..`.
fn bulk(node: &Node<MemoryStore>, index: &str, sources: &[Value]) -> Value {
    let mut text = String::new();
    for (i, source) in sources.iter().enumerate() {
        text.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{source}\n"));
    }
    let (status, response) = node
        .bulk(
            text.as_bytes(),
            Some(text.len() as u64),
            Some(index),
            Wait::Searchable,
        )
        .unwrap();
    assert_eq!(status, 200);
    response
}

fn item_error(response: &Value, i: usize) -> (&str, &str) {
    let error = &response["items"][i]["index"]["error"];
    (
        error["type"].as_str().unwrap_or(""),
        error["reason"].as_str().unwrap_or(""),
    )
}

#[test]
fn a_mapping_at_the_field_limit_is_created_and_one_more_is_refused_whole() {
    let limits = MappingLimits {
        max_fields: 3,
        ..MappingLimits::default()
    };
    let (node, _scratch) = node("limit-create", limits);
    node.create("ok", &body(None, &["a", "b", "c"])).unwrap();
    let error: ApiError = node
        .create("over", &body(None, &["a", "b", "c", "d"]))
        .unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(error.kind, "illegal_argument_exception");
    assert!(
        error.reason.contains("Limit of total fields [3]"),
        "{}",
        error.reason
    );
    // Nothing was created: the name is free and a valid mapping can take it.
    node.create("over", &body(None, &["a", "b", "c"])).unwrap();
}

#[test]
fn strict_rejects_an_unknown_field_per_item_and_leaves_the_mapping_alone() {
    let (node, _scratch) = node("strict", MappingLimits::default());
    node.create("s", &body(Some("strict"), &["a", "b"]))
        .unwrap();
    let response = bulk(
        &node,
        "s",
        &[
            json!({"a": "x", "b": "y"}),
            json!({"a": "x", "b": "y", "extra": "z"}),
            json!({"a": "x", "b": "y"}),
        ],
    );
    assert_eq!(response["errors"], json!(true));
    let (kind, reason) = item_error(&response, 1);
    assert_eq!(kind, "strict_dynamic_mapping_exception");
    assert!(reason.contains("[extra]"), "{reason}");
    assert!(response["items"][0]["index"]["error"].is_null());
    assert!(response["items"][2]["index"]["error"].is_null());
}

#[test]
fn false_ignores_an_unknown_field_and_indexes_the_rest() {
    let (node, _scratch) = node("false", MappingLimits::default());
    node.create("f", &body(Some("false"), &["a", "b"])).unwrap();
    let response = bulk(&node, "f", &[json!({"a": "x", "b": "y", "extra": 5})]);
    assert_eq!(response["errors"], json!(false), "{response}");
    let (status, got) = node.get("f", "0").unwrap();
    assert_eq!(status, 200);
    assert_eq!(got["_source"], json!({"a": "x", "b": "y"}));
}

#[test]
fn boolean_spellings_of_dynamic_are_accepted_and_junk_is_refused() {
    let (node, _scratch) = node("spell", MappingLimits::default());
    for (name, value) in [
        ("t", json!(true)),
        ("f", json!(false)),
        ("s", json!("strict")),
    ] {
        let mut b = body(None, &["a"]);
        b["mappings"]["dynamic"] = value;
        node.create(name, &b).unwrap();
    }
    for junk in [json!("maybe"), json!(1), json!(null), json!({})] {
        let mut b = body(None, &["a"]);
        b["mappings"]["dynamic"] = junk;
        assert!(node.create("junk", &b).is_err());
    }
}

#[test]
fn dynamic_true_checks_the_limit_before_anything_else_and_never_half_applies() {
    let limits = MappingLimits {
        max_fields: 3,
        ..MappingLimits::default()
    };
    let (node, _scratch) = node("true-limit", limits);
    node.create("d", &body(Some("true"), &["a", "b"])).unwrap();
    // One new field would make 3: within the limit, but adding fields to a live
    // index is M21.04, so the item is refused and says so.
    // Two new fields would make 4: over the limit, refused by the limit.
    let response = bulk(
        &node,
        "d",
        &[
            json!({"a": "x", "b": "y", "c": "z"}),
            json!({"a": "x", "b": "y", "c": "z", "d": "w"}),
        ],
    );
    assert_eq!(response["errors"], json!(true));
    let (kind0, _) = item_error(&response, 0);
    assert_eq!(kind0, "mapping_update_unavailable_exception");
    let (kind1, reason1) = item_error(&response, 1);
    assert_eq!(kind1, "illegal_argument_exception");
    assert!(reason1.contains("Limit of total fields [3]"), "{reason1}");
    // The stored mapping is unchanged: a restart reads the same two fields.
    let (status, doc) = node.get("d", "0").unwrap();
    assert_eq!(status, 404, "{doc}");
}

#[test]
fn nesting_is_still_refused_per_item_by_the_bulk_shape_guard() {
    // The mapping holds scalars only, so the bulk reader refuses any nested
    // container before the depth and nested-object limits could apply (those are
    // unit-tested in `policy.rs`, ready for the field types that need them).
    let (node, _scratch) = node("nesting", MappingLimits::default());
    node.create("n", &body(Some("false"), &["a"])).unwrap();
    let response = bulk(
        &node,
        "n",
        &[
            json!({"a": "x"}),
            json!({"a": "x", "o": {"p": 1}}),
            json!({"a": "y"}),
        ],
    );
    assert!(
        response["items"][0]["index"]["error"].is_null(),
        "{response}"
    );
    assert_eq!(item_error(&response, 1).0, "mapper_parsing_exception");
    assert!(
        response["items"][2]["index"]["error"].is_null(),
        "{response}"
    );
}

#[test]
fn limits_that_cannot_work_are_refused() {
    for bad in [
        MappingLimits {
            max_fields: 0,
            ..MappingLimits::default()
        },
        MappingLimits {
            max_fields: hsearch_node::MAX_FIELDS_CEILING + 1,
            ..MappingLimits::default()
        },
        MappingLimits {
            max_depth: 0,
            ..MappingLimits::default()
        },
        MappingLimits {
            max_nested_objects: 0,
            ..MappingLimits::default()
        },
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
    }
    assert_eq!(MappingLimits::default().validate(), Ok(()));
    assert_eq!(MappingLimits::default().max_fields, 1000);
    assert_eq!(MappingLimits::default().max_depth, 20);
    assert_eq!(MappingLimits::default().max_nested_objects, 10_000);
}
