//! A large text field is indexed whole, found by a term at its end, and returned
//! intact; a document over the memory budget is refused by name.
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
fn a_document_past_the_inline_size_is_stored_as_chunk_objects_and_found() {
    // 10 MiB: over the 1 MiB inline slot limit, so the log holds a reference and
    // the text lives in two chunk objects.
    let roomy = Limits {
        memory_budget_bytes: 512 << 20,
        ..limits()
    };
    let (store, dir) = (store(), Scratch::new("chunked"));
    let node = Arc::new(Node::with_limits(store.clone(), dir.0.clone(), roomy).unwrap());
    let running = serve(node, "127.0.0.1:0", 2, false).unwrap();
    let addr = running.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    let title = format!("{}zebra", "x ".repeat(5 << 20));
    let (status, out) = call(
        addr,
        "POST",
        "/orders/_bulk?refresh=true",
        &action("huge", &title),
    );
    assert_eq!(
        (status, item(&out)["status"].as_u64()),
        (200, Some(201)),
        "{out}"
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
fn a_document_the_budget_cannot_hold_is_refused_with_429_and_commits_nothing() {
    // The default budget in this file (256 MiB) is below the 32x charge of 10 MiB.
    let (store, dir) = (store(), Scratch::new("pastbudget"));
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
    assert_eq!(item(&out)["status"], json!(429), "{out}");
    let reason = item(&out)["error"]["reason"].as_str().unwrap();
    assert!(
        reason.contains("bytes free") && reason.contains("needs"),
        "{reason}"
    );
    assert_eq!(call(addr, "GET", "/orders/_doc/huge", "").0, 404);
    running.shutdown();
}

#[test]
fn a_large_document_is_charged_its_indexing_memory_and_refused_per_item_when_the_budget_is_spent() {
    // A budget that admits one full request but not the indexing working set of
    // a 1 MiB document on top of it: the batch is refused item by item with
    // 429 and nothing of it is committed.
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

#[test]
fn the_charge_is_released_after_each_batch_and_a_mixed_batch_is_refused_whole() {
    // One 1 MiB document's charge fits the budget, but not two at once.
    let one_at_a_time = Limits {
        buffer_bytes: 8 << 20,
        memory_budget_bytes: 40 << 20,
        ..limits()
    };
    let (store, dir) = (store(), Scratch::new("released"));
    let node = Arc::new(Node::with_limits(store, dir.0.clone(), one_at_a_time).unwrap());
    let running = serve(node, "127.0.0.1:0", 2, false).unwrap();
    let addr = running.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    let title = "x ".repeat(512 * 1024);
    // Sequential requests each succeed: the first request's charge was returned.
    for id in ["a", "b", "c"] {
        let (_, out) = call(addr, "POST", "/orders/_bulk", &action(id, &title));
        assert_eq!(item(&out)["status"], json!(201), "{id}: {out}");
    }
    // Two large documents and a small one in one batch need two charges at
    // once: the whole batch is refused, the small item included.
    let body = [action("d", &title), action("e", &title), doc(7)].concat();
    let (status, out) = call(addr, "POST", "/orders/_bulk", &body);
    assert_eq!(status, 200, "{out}");
    let statuses: Vec<_> = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i.as_object().unwrap().values().next().unwrap()["status"].clone())
        .collect();
    assert_eq!(statuses, vec![json!(429); 3], "{out}");
    running.shutdown();
}

#[test]
fn a_keyword_over_lucenes_term_limit_is_refused_through_the_node() {
    let (store, dir) = (store(), Scratch::new("keyword"));
    let node = Arc::new(Node::with_limits(store, dir.0.clone(), limits()).unwrap());
    let running = serve(node, "127.0.0.1:0", 2, false).unwrap();
    let addr = running.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    let body = |tag: usize| {
        format!(
            "{{\"index\":{{\"_id\":\"k{tag}\"}}}}\n{{\"title\":\"t\",\"tag\":\"{}\",\"n\":1,\"ok\":true,\"at\":1}}\n",
            "k".repeat(tag)
        )
    };
    let (_, out) = call(addr, "POST", "/orders/_bulk", &body(32_766));
    assert_eq!(item(&out)["status"], json!(201), "{out}");
    let (_, out) = call(addr, "POST", "/orders/_bulk", &body(32_767));
    assert_eq!(item(&out)["status"], json!(400), "{out}");
    assert_eq!(
        item(&out)["error"]["type"],
        json!("limit_exceeded_exception")
    );
    running.shutdown();
}
