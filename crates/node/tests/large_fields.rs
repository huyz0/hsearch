//! A large text field is indexed whole, found by a term at its end, and returned
//! intact; a document past what this build can index is refused by name.
mod support;

use std::sync::Arc;

use hsearch_node::{Limits, Node, serve};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use serde_json::{Value, json};
use support::*;

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

fn limits() -> Limits {
    Limits {
        max_document_bytes: 100 << 20,
        max_bulk_bytes: 128 << 20,
        max_bulk_items: 100,
        buffer_bytes: 16 << 20,
        memory_budget_bytes: 256 << 20,
    }
}

fn action(id: &str, title: &str) -> String {
    format!(
        "{{\"index\":{{\"_id\":\"{id}\"}}}}\n{{\"title\":\"{title}\",\"tag\":\"a\",\"n\":1,\"ok\":true,\"at\":1}}\n"
    )
}

fn item(out: &Value) -> &Value {
    &out["items"][0]["index"]
}

#[test]
fn a_megabytes_long_text_field_is_indexed_searched_and_returned_whole() {
    let (store, dir) = (store(), Scratch::new("largefield"));
    let node = Arc::new(Node::with_limits(store, dir.0.clone(), limits()).unwrap());
    let running = serve(node, "127.0.0.1:0", 2, false).unwrap();
    let addr = running.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);

    let title = format!("{}zebra", "lorem ipsum ".repeat(200_000));
    let (status, out) = call(
        addr,
        "POST",
        "/orders/_bulk?refresh=true",
        &action("big", &title),
    );
    assert_eq!(
        (status, item(&out)["status"].as_u64()),
        (200, Some(201)),
        "{out}"
    );

    let (_, got) = call(addr, "GET", "/orders/_doc/big", "");
    assert_eq!(
        got["_source"]["title"].as_str().map(str::len),
        Some(title.len())
    );
    let (_, found) = call(
        addr,
        "POST",
        "/orders/_search",
        &json!({"query": {"match": {"title": "zebra"}}, "_source": ["n"]}).to_string(),
    );
    assert_eq!(found["hits"]["total"]["value"], json!(1), "{found}");
    running.shutdown();
}

#[test]
fn a_document_past_what_the_build_can_index_is_refused_by_name_and_commits_nothing() {
    let (store, dir) = (store(), Scratch::new("pastbuild"));
    let node = Arc::new(Node::with_limits(store, dir.0.clone(), limits()).unwrap());
    let running = serve(node, "127.0.0.1:0", 2, false).unwrap();
    let addr = running.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);

    let (status, out) = call(
        addr,
        "POST",
        "/orders/_bulk?refresh=true",
        &action("huge", &"x ".repeat(5 << 20)),
    );
    assert_eq!(status, 200, "{out}");
    assert_eq!(item(&out)["status"], json!(413), "{out}");
    assert_eq!(
        item(&out)["error"]["type"],
        json!("document_too_large_exception")
    );
    let reason = item(&out)["error"]["reason"].as_str().unwrap();
    assert!(reason.contains("can index"), "{reason}");
    assert_eq!(call(addr, "GET", "/orders/_doc/huge", "").0, 404);
    running.shutdown();
}

#[test]
fn a_large_document_is_charged_its_indexing_memory_and_refused_per_item_when_the_budget_is_spent() {
    // A budget that admits one full request but not the indexing working set of
    // a 1 MiB document on top of it: the item is refused with 429, the rest of
    // the bulk is not, and the document is not committed.
    let tight = Limits {
        max_document_bytes: 100 << 20,
        max_bulk_bytes: 128 << 20,
        max_bulk_items: 100,
        buffer_bytes: 2 << 20,
        memory_budget_bytes: 3 * (2 << 20) + (64 << 10),
    };
    let (store, dir) = (store(), Scratch::new("charged"));
    let node = Arc::new(Node::with_limits(store, dir.0.clone(), tight).unwrap());
    let running = serve(node, "127.0.0.1:0", 2, false).unwrap();
    let addr = running.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    let body = action("big", &"x ".repeat(512 * 1024));
    let (status, out) = call(addr, "POST", "/orders/_bulk?refresh=true", &body);
    assert_eq!(status, 200, "{out}");
    assert_eq!(item(&out)["status"], json!(429), "{out}");
    assert_eq!(
        item(&out)["error"]["type"],
        json!("es_rejected_execution_exception")
    );
    assert_eq!(call(addr, "GET", "/orders/_doc/big", "").0, 404);
    // Small documents are not charged extra and still go through.
    let (status, out) = call(addr, "POST", "/orders/_bulk?refresh=true", &doc(1));
    assert_eq!(
        (status, item(&out)["status"].as_u64()),
        (200, Some(201)),
        "{out}"
    );
    running.shutdown();
}
