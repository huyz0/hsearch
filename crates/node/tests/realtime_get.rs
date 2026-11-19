//! Real-time get and multi-get from the shard owner (M21.05, PR-49 owner side):
//! a read returns the latest acknowledged write whether or not a refresh has made
//! it searchable, `realtime=false` reads what is searchable, and a multi-get costs
//! what the shards it touches cost, not what the documents it asks for cost.
#[path = "support/counting.rs"]
mod counting;
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use counting::Counting;
use hsearch_index::Wait;
use hsearch_node::{Limits, Node};
use serde_json::{Value, json};
use support::Scratch;

fn node_on(store: &Arc<Counting>, dir: &std::path::Path) -> Node<Counting> {
    Node::with_limits(Arc::clone(store), dir.to_path_buf(), Limits::default()).unwrap()
}

fn create(node: &Node<Counting>) {
    node.create(
        "ix",
        &json!({"mappings": {"dynamic": "strict", "properties": {
            "title": {"type": "text"}, "n": {"type": "integer"}}}}),
    )
    .unwrap();
}

fn write(node: &Node<Counting>, lines: &[String], wait: Wait) {
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let (status, body) = node
        .bulk(text.as_bytes(), Some(text.len() as u64), Some("ix"), wait)
        .unwrap();
    assert_eq!((status, &body["errors"]), (200, &json!(false)), "{body}");
}

/// Index `id` with `n`, acknowledged once durable and not searchable.
fn put(node: &Node<Counting>, id: &str, n: i64) {
    write(
        node,
        &[
            format!("{{\"index\":{{\"_id\":\"{id}\"}}}}"),
            format!("{{\"title\":\"t{id}\",\"n\":{n}}}"),
        ],
        Wait::Durable,
    );
}

fn delete(node: &Node<Counting>, id: &str) {
    write(
        node,
        &[format!("{{\"delete\":{{\"_id\":\"{id}\"}}}}")],
        Wait::Durable,
    );
}

fn searchable(node: &Node<Counting>) -> u64 {
    node.search("ix", &json!({"size": 0})).unwrap()["hits"]["total"]["value"]
        .as_u64()
        .unwrap()
}

#[test]
fn a_write_not_yet_searchable_is_read_at_once() {
    let (store, dir) = (Counting::new(), Scratch::new("rt-basic"));
    let node = node_on(&store, &dir.0);
    create(&node);
    put(&node, "1", 7);
    assert_eq!(searchable(&node), 0, "the write is durable but unpublished");

    let (status, found) = node.get("ix", "1").unwrap();
    assert_eq!(status, 200, "{found}");
    assert_eq!(found["found"], json!(true));
    assert_eq!(found["_source"], json!({"n": 7, "title": "t1"}));

    // realtime=false reads what is searchable: nothing yet.
    let (status, stale) = node.get_with("ix", "1", false).unwrap();
    assert_eq!((status, &stale["found"]), (404, &json!(false)), "{stale}");

    node.refresh("ix").unwrap();
    let (_, after) = node.get_with("ix", "1", false).unwrap();
    assert_eq!(after["found"], json!(true));
    assert_eq!(after["_version"], found["_version"]);
}

#[test]
fn an_update_then_a_delete_are_each_read_at_once() {
    let (store, dir) = (Counting::new(), Scratch::new("rt-update"));
    let node = node_on(&store, &dir.0);
    create(&node);
    put(&node, "1", 1);
    node.refresh("ix").unwrap();
    put(&node, "1", 2);
    let (_, updated) = node.get("ix", "1").unwrap();
    assert_eq!(updated["_source"]["n"], json!(2), "{updated}");
    // The published view is still the old document.
    let (_, old) = node.get_with("ix", "1", false).unwrap();
    assert_eq!(old["_source"]["n"], json!(1));
    assert!(updated["_version"].as_i64() > old["_version"].as_i64());

    delete(&node, "1");
    let (status, gone) = node.get("ix", "1").unwrap();
    assert_eq!((status, &gone["found"]), (404, &json!(false)), "{gone}");
    // And it can come back.
    put(&node, "1", 3);
    let (status, back) = node.get("ix", "1").unwrap();
    assert_eq!((status, &back["_source"]["n"]), (200, &json!(3)));
}

#[test]
fn the_same_holds_after_an_owner_restart() {
    let (store, one, two) = (
        Counting::new(),
        Scratch::new("rt-r1"),
        Scratch::new("rt-r2"),
    );
    {
        let node = node_on(&store, &one.0);
        create(&node);
        put(&node, "keep", 1);
        put(&node, "change", 1);
        put(&node, "change", 2);
        put(&node, "drop", 1);
        delete(&node, "drop");
    }
    let node = node_on(&store, &two.0);
    let (_, kept) = node.get("ix", "keep").unwrap();
    assert_eq!(kept["_source"]["n"], json!(1));
    let (_, changed) = node.get("ix", "change").unwrap();
    assert_eq!(changed["_source"]["n"], json!(2));
    assert_eq!(node.get("ix", "drop").unwrap().0, 404);
    // A write after the restart is read at once too.
    put(&node, "keep", 9);
    assert_eq!(node.get("ix", "keep").unwrap().1["_source"]["n"], json!(9));
}

#[test]
fn a_document_written_before_a_field_existed_is_read_with_the_field_absent() {
    let (store, dir) = (Counting::new(), Scratch::new("rt-absent"));
    let node = node_on(&store, &dir.0);
    create(&node);
    put(&node, "old", 1);
    node.put_mapping("ix", &json!({"properties": {"tag": {"type": "keyword"}}}))
        .unwrap();
    // Written under the new mapping without the added field, and with it.
    put(&node, "bare", 2);
    write(
        &node,
        &[
            "{\"index\":{\"_id\":\"tagged\"}}".to_string(),
            "{\"title\":\"x\",\"n\":3,\"tag\":\"hot\"}".to_string(),
        ],
        Wait::Durable,
    );
    for (id, tag) in [("old", None), ("bare", None), ("tagged", Some("hot"))] {
        let (status, doc) = node.get("ix", id).unwrap();
        assert_eq!(status, 200, "{doc}");
        assert_eq!(
            doc["_source"].get("tag"),
            tag.map(|t| json!(t)).as_ref(),
            "{doc}"
        );
    }
}

#[test]
fn a_multi_get_costs_the_same_for_ten_documents_as_for_twenty() {
    let (store, dir) = (Counting::new(), Scratch::new("rt-mget"));
    let node = node_on(&store, &dir.0);
    create(&node);
    for i in 0..20 {
        put(&node, &i.to_string(), i);
    }
    // Half published, half only durable.
    node.refresh("ix").unwrap();
    for i in 10..20 {
        put(&node, &i.to_string(), i + 100);
    }
    let ask = |count: usize| {
        let ids: Vec<String> = (0..count).map(|i| i.to_string()).collect();
        store.take();
        let (status, body) = node.mget(Some("ix"), &json!({"ids": ids}), true).unwrap();
        assert_eq!(status, 200, "{body}");
        let cost = store.take();
        (body, cost)
    };
    let (ten, cost_ten) = ask(10);
    let (twenty, cost_twenty) = ask(20);
    assert_eq!(ten["docs"].as_array().unwrap().len(), 10);
    assert_eq!(twenty["docs"].as_array().unwrap().len(), 20);
    assert_eq!(
        cost_ten, cost_twenty,
        "requests do not follow the document count"
    );
    assert!(
        cost_twenty.is_empty(),
        "a get reads the owner's state: {cost_twenty:?}"
    );
    // The durable-only half is the updated copy, in the order asked.
    for (i, doc) in twenty["docs"].as_array().unwrap().iter().enumerate() {
        let expected = if i >= 10 { i as i64 + 100 } else { i as i64 };
        assert_eq!(doc["_id"], json!(i.to_string()));
        assert_eq!(doc["_source"]["n"], json!(expected), "{doc}");
        assert_eq!(doc["found"], json!(true));
    }
}

#[test]
fn a_multi_get_reports_each_document_alone() {
    let (store, dir) = (Counting::new(), Scratch::new("rt-mget-mixed"));
    let node = node_on(&store, &dir.0);
    create(&node);
    put(&node, "a", 1);
    put(&node, "b", 2);
    delete(&node, "b");
    let (_, body) = node
        .mget(
            None,
            &json!({"docs": [
                {"_index": "ix", "_id": "a"}, {"_index": "ix", "_id": "b"},
                {"_index": "ix", "_id": "zz"}, {"_index": "ix", "_id": "a", "_routing": "r"},
                {"_index": "nope", "_id": "a"}, {"_id": "a"}]}),
            true,
        )
        .unwrap();
    let docs = body["docs"].as_array().unwrap();
    let found: Vec<_> = docs.iter().map(|d| d.get("found").cloned()).collect();
    assert_eq!(
        found,
        vec![
            Some(json!(true)),
            Some(json!(false)),
            Some(json!(false)),
            Some(json!(true)),
            None,
            None
        ],
        "{body}"
    );
    // A missing index or a missing index name is that item's error, not the request's.
    assert_eq!(docs[4]["error"]["type"], json!("index_not_found_exception"));
    assert_eq!(
        docs[5]["error"]["type"],
        json!("action_request_validation_exception")
    );
    // A shape that is no request at all is refused.
    assert!(node.mget(Some("ix"), &json!({"ids": [1]}), true).is_err());
    assert!(node.mget(Some("ix"), &json!({}), true).is_err());
    assert!(
        node.mget(Some("ix"), &json!({"ids": ["a"], "docs": []}), true)
            .is_err()
    );
}

#[test]
fn a_get_never_returns_older_data_than_an_acknowledged_write() {
    let (store, dir) = (Counting::new(), Scratch::new("rt-race"));
    let node = Arc::new(node_on(&store, &dir.0));
    create(&node);
    let acked = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let (node, acked, stop) = (Arc::clone(&node), Arc::clone(&acked), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !stop.load(Ordering::Acquire) {
                    // Read the acknowledged floor first, then the document.
                    let floor = acked.load(Ordering::Acquire) as i64;
                    let (_, doc) = node.get("ix", "hot").unwrap();
                    let seen = doc["_source"]["n"].as_i64().unwrap_or(0);
                    assert!(seen >= floor, "read {seen} after {floor} was acknowledged");
                    reads += 1;
                }
                reads
            })
        })
        .collect();
    for n in 1..=300i64 {
        put(&node, "hot", n);
        acked.store(n as u64, Ordering::Release);
    }
    stop.store(true, Ordering::Release);
    let reads: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(reads > 0);
}

#[test]
fn the_owner_keeps_bounded_state_for_writes_nobody_has_refreshed() {
    let (store, dir) = (Counting::new(), Scratch::new("rt-bound"));
    let node = node_on(&store, &dir.0);
    create(&node);
    // Far more unrefreshed writes than the owner will hold for real-time reads;
    // the oldest and the newest are both still read correctly.
    for chunk in 0..40 {
        let lines: Vec<String> = (0..1000)
            .flat_map(|i| {
                let id = chunk * 1000 + i;
                [
                    format!("{{\"index\":{{\"_id\":\"{id}\"}}}}"),
                    format!("{{\"title\":\"t\",\"n\":{id}}}"),
                ]
            })
            .collect();
        write(&node, &lines, Wait::Durable);
    }
    for id in [0, 1, 20_000, 39_999] {
        let (status, doc) = node.get("ix", &id.to_string()).unwrap();
        assert_eq!((status, &doc["_source"]["n"]), (200, &json!(id)), "{doc}");
    }
    assert_eq!(node.get("ix", "40000").unwrap().0, 404);
}

#[test]
fn the_http_routes_take_realtime_and_routing() {
    use hsearch_node::serve;
    let (store, dir) = (Counting::new(), Scratch::new("rt-http"));
    let node = Arc::new(node_on(&store, &dir.0));
    let running = serve(Arc::clone(&node), "127.0.0.1:0", 2, false).unwrap();
    let addr = running.addr;
    create(&node);
    put(&node, "1", 5);
    let (status, doc) = support::call(addr, "GET", "/ix/_doc/1", "");
    assert_eq!((status, &doc["found"]), (200, &json!(true)), "{doc}");
    let (status, doc) = support::call(addr, "GET", "/ix/_doc/1?realtime=false", "");
    assert_eq!((status, &doc["found"]), (404, &json!(false)), "{doc}");
    let (status, doc) = support::call(addr, "GET", "/ix/_doc/1?routing=abc", "");
    assert_eq!((status, &doc["found"]), (200, &json!(true)), "{doc}");
    let (status, _) = support::call(addr, "GET", "/ix/_doc/1?realtime=maybe", "");
    assert_eq!(status, 400);
    let (status, many) = support::call(addr, "POST", "/ix/_mget", r#"{"ids":["1","2"]}"#);
    assert_eq!(status, 200, "{many}");
    assert_eq!(many["docs"][0]["found"], json!(true));
    assert_eq!(many["docs"][1]["found"], json!(false));
    let (status, many) = support::call(
        addr,
        "POST",
        "/_mget?realtime=false",
        r#"{"docs":[{"_index":"ix","_id":"1"}]}"#,
    );
    assert_eq!(
        (status, &many["docs"][0]["found"]),
        (200, &json!(false)),
        "{many}"
    );
    let _: Value = many;
    running.shutdown();
}
