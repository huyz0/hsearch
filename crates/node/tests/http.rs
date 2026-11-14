//! The HTTP subset end to end over the in-memory store.
mod support;

use hsearch_node::{Limits, MAX_BULK_BYTES, MAX_BULK_ITEMS, Node, serve};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use serde_json::{Value, json};
use std::sync::Arc;
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

fn start(store: &Arc<MemoryStore>, dir: &Scratch) -> hsearch_node::Running {
    let node = Arc::new(Node::new(Arc::clone(store), dir.0.clone()));
    serve(node, "127.0.0.1:0", 4, false).unwrap()
}

fn ids(body: &Value) -> Vec<String> {
    body["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap().to_string())
        .collect()
}

fn load(addr: std::net::SocketAddr, n: usize) {
    let bulk: String = (0..n).map(doc).collect();
    let (status, body) = call(addr, "POST", "/orders/_bulk?refresh=true", &bulk);
    assert_eq!((status, &body["errors"]), (200, &json!(false)), "{body}");
}

#[test]
fn create_bulk_get_search_refresh_and_delete() {
    let (store, dir) = (store(), Scratch::new("flow"));
    let node = start(&store, &dir);
    let addr = node.addr;
    let (status, body) = call(addr, "PUT", "/orders", MAPPING);
    assert_eq!(
        (status, body["acknowledged"].clone()),
        (200, json!(true)),
        "{body}"
    );
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 400);

    let bulk: String = (0..30).map(doc).collect();
    let (status, body) = call(addr, "POST", "/orders/_bulk?refresh=true", &bulk);
    assert_eq!(status, 200);
    assert_eq!(body["errors"], json!(false));
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 30);
    assert_eq!(items[0]["index"]["result"], json!("created"));
    assert_eq!(items[0]["index"]["status"], json!(201));
    assert_eq!(items[0]["index"]["_id"], json!("d0"));

    let (status, got) = call(addr, "GET", "/orders/_doc/d7", "");
    assert_eq!(status, 200);
    assert_eq!(got["found"], json!(true));
    assert_eq!(got["_source"]["n"], json!(7));
    assert_eq!(got["_source"]["ok"], json!(false));
    assert_eq!(got["_source"]["tag"], json!("t1"));
    assert_eq!(call(addr, "GET", "/orders/_doc/nope", "").0, 404);

    let search = |body: Value| call(addr, "POST", "/orders/_search", &body.to_string());
    let (_, all) = search(json!({"query": {"match_all": {}}, "size": 0}));
    assert_eq!(all["hits"]["total"], json!({"value": 30, "relation": "eq"}));
    let (_, term) = search(json!({"query": {"term": {"tag": "t0"}}, "size": 0}));
    assert_eq!(term["hits"]["total"]["value"], json!(10));
    let (_, range) = search(json!({"query": {"range": {"n": {"gte": 5, "lt": 9}}},
                                   "sort": [{"n": "asc"}], "size": 10}));
    assert_eq!(ids(&range), vec!["d5", "d6", "d7", "d8"]);
    let (_, exclusive) = search(json!({"query": {"range": {"n": {"gt": 5, "lte": 9}}},
                                       "sort": [{"n": {"order": "desc"}}]}));
    assert_eq!(ids(&exclusive), vec!["d9", "d8", "d7", "d6"]);
    let (_, text) = search(json!({"query": {"match": {"title": "fox 12"}}, "size": 50}));
    assert!(ids(&text).contains(&"d12".to_string()));
    let (_, phrase) = search(json!({"query": {"match_phrase": {"title": "brown fox 12"}}}));
    assert_eq!(ids(&phrase), vec!["d12"]);
    let (_, boolean) = search(json!({"query": {"bool": {
        "filter": [{"term": {"tag": "t1"}}, {"range": {"n": {"lte": 12}}}],
        "must_not": [{"term": {"ok": true}}]}}, "sort": [{"n": "asc"}]}));
    assert_eq!(ids(&boolean), vec!["d1", "d7"]);
    let (_, should) = search(json!({"query": {"bool": {"should": [
        {"term": {"n": 1}}, {"term": {"n": 2}}]}}, "sort": ["n"]}));
    assert_eq!(ids(&should), vec!["d1", "d2"]);
    let (_, terms) = search(json!({"query": {"terms": {"n": [3, 4]}}, "sort": [{"n": "desc"}]}));
    assert_eq!(ids(&terms), vec!["d4", "d3"]);
    let (_, dates) = search(json!({"query": {"range": {"at": {"gte": "2026-01-08"}}},
                                   "size": 0}));
    assert!(dates["hits"]["total"]["value"].as_u64().unwrap() > 0);
    // Paging and projection.
    let (_, page) = search(json!({"query": {"match_all": {}}, "sort": [{"n": "asc"}],
                                  "from": 5, "size": 3, "_source": ["n"]}));
    assert_eq!(ids(&page), vec!["d5", "d6", "d7"]);
    let first = &page["hits"]["hits"][0];
    assert_eq!(first["_source"], json!({"n": 5}));
    assert_eq!(first["_score"], Value::Null);
    let (_, bare) = search(json!({"size": 1, "_source": false}));
    assert!(bare["hits"]["hits"][0].get("_source").is_none());

    // Update, delete, and a delete of a missing id.
    let mutate = "{\"index\":{\"_id\":\"d1\"}}\n{\"title\":\"changed\",\"tag\":\"zz\",\"n\":1,\"ok\":true,\"at\":1000}\n{\"delete\":{\"_id\":\"d2\"}}\n{\"delete\":{\"_id\":\"ghost\"}}\n";
    let (_, body) = call(addr, "POST", "/orders/_bulk", mutate);
    let results: Vec<_> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            let item = i.as_object().unwrap().values().next().unwrap();
            (
                item["result"].as_str().unwrap().to_string(),
                item["status"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        results,
        vec![
            ("updated".into(), 200),
            ("deleted".into(), 200),
            ("not_found".into(), 404)
        ]
    );
    // Without refresh the old view still answers; after a refresh the new one does.
    assert_eq!(call(addr, "GET", "/orders/_doc/d2", "").0, 200);
    let (status, body) = call(addr, "POST", "/orders/_refresh", "");
    assert_eq!((status, body["_shards"]["failed"].clone()), (200, json!(0)));
    assert_eq!(call(addr, "GET", "/orders/_doc/d2", "").0, 404);
    let (_, got) = call(addr, "GET", "/orders/_doc/d1", "");
    assert_eq!(got["_source"]["tag"], json!("zz"));
    assert_eq!(got["_source"]["at"], json!(1000));
    let (_, all) = search(json!({"size": 0}));
    assert_eq!(all["hits"]["total"]["value"], json!(29));
    node.shutdown();
}

#[test]
fn a_restarted_node_serves_the_same_answers_from_the_store_alone() {
    let (store, first, second) = (store(), Scratch::new("r1"), Scratch::new("r2"));
    let node = start(&store, &first);
    assert_eq!(call(node.addr, "PUT", "/orders", MAPPING).0, 200);
    load(node.addr, 60);
    let query = json!({"query": {"term": {"tag": "t2"}}, "sort": [{"n": "asc"}], "size": 100});
    let (_, before) = call(node.addr, "POST", "/orders/_search", &query.to_string());
    node.shutdown();
    drop(first);
    // A fresh node, a fresh data directory, the same object store.
    let node = start(&store, &second);
    let (status, after) = call(node.addr, "POST", "/orders/_search", &query.to_string());
    assert_eq!(status, 200);
    assert_eq!(ids(&after), ids(&before));
    assert_eq!(after["hits"]["total"], before["hits"]["total"]);
    assert_eq!(call(node.addr, "GET", "/orders/_doc/d5", "").0, 200);
    // Writes continue after the restart.
    let (_, body) = call(
        node.addr,
        "POST",
        "/orders/_bulk?refresh=wait_for",
        &doc(500),
    );
    assert_eq!(body["errors"], json!(false));
    node.shutdown();
}

#[test]
fn routes_parameters_and_bodies_outside_the_subset_are_refused_by_name() {
    let (store, dir) = (store(), Scratch::new("refuse"));
    let node = start(&store, &dir);
    let addr = node.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    load(addr, 5);
    let kind = |(status, body): (u16, Value)| {
        (
            status,
            body["error"]["type"].as_str().unwrap_or("").to_string(),
        )
    };
    assert_eq!(
        kind(call(addr, "GET", "/", "")),
        (404, "no_handler_found_exception".into())
    );
    assert_eq!(kind(call(addr, "GET", "/orders/_mapping", "")).0, 404);
    assert_eq!(kind(call(addr, "DELETE", "/orders", "")).0, 405);
    assert_eq!(kind(call(addr, "GET", "/orders/_bulk", "")).0, 405);
    assert_eq!(kind(call(addr, "PUT", "/orders/_doc/d1", "{}")).0, 405);
    assert_eq!(
        kind(call(addr, "POST", "/orders/_search?pretty=true", "{}")).0,
        400
    );
    assert_eq!(
        kind(call(addr, "POST", "/orders/_bulk?routing=x", "")).0,
        400
    );
    assert_eq!(
        kind(call(addr, "POST", "/orders/_bulk?refresh=maybe", "")).0,
        400
    );
    assert_eq!(
        kind(call(addr, "GET", "/nope/_search", "{}")),
        (404, "index_not_found_exception".into())
    );
    assert_eq!(kind(call(addr, "GET", "/Bad%20Name/_search", "{}")).0, 400);
    assert_eq!(kind(call(addr, "GET", "/orders/_doc/%zz", "")).0, 400);
    assert_eq!(kind(call(addr, "PUT", "/_orders", MAPPING)).0, 404);
    // `v1` is reserved: its prefix holds the shared objects.
    assert_eq!(kind(call(addr, "PUT", "/v1", MAPPING)).0, 400);
    assert_eq!(kind(call(addr, "PUT", "/newidx", "not json")).0, 400);
    assert_eq!(
        kind(call(
            addr,
            "PUT",
            "/newidx",
            r#"{"mappings":{"properties":{"a":{"type":"geo_point"}}}}"#
        ))
        .0,
        400
    );
    let search = |body: Value| kind(call(addr, "POST", "/orders/_search", &body.to_string()));
    for refused in [
        json!({"aggs": {}}),
        json!({"query": {"fuzzy": {"title": "x"}}}),
        json!({"query": {"match_all": {}, "term": {"tag": "a"}}}),
        json!({"query": {"term": {"nofield": "a"}}}),
        json!({"query": {"term": {"n": "text"}}}),
        json!({"query": {"range": {"tag": {"gte": "a"}}}}),
        json!({"query": {"range": {"n": {"gte": 1, "gt": 1}}}}),
        json!({"query": {"range": {"n": {"format": "x"}}}}),
        json!({"query": {"match": {"title": {"query": "x", "fuzziness": 1}}}}),
        json!({"query": {"bool": {"minimum_should_match": "50%"}}}),
        json!({"query": {"bool": {"boost": 1}}}),
        json!({"size": 1001}),
        json!({"from": 995, "size": 6}),
        json!({"size": -1}),
        json!({"sort": [{"title": "asc"}]}),
        json!({"sort": [{"n": "sideways"}]}),
        json!({"sort": [1, 2, 3, 4, 5]}),
        json!({"_source": "n"}),
        json!({"_source": ["n", "n"]}),
        json!({"_source": ["ghost"]}),
        json!([]),
    ] {
        let (status, _) = search(refused.clone());
        assert_eq!(status, 400, "{refused}");
    }
    let mut deep = json!({"match_all": {}});
    for _ in 0..12 {
        deep = json!({"bool": {"must": [deep]}});
    }
    assert_eq!(search(json!({"query": deep})).0, 400);
    node.shutdown();
}

#[test]
fn bulk_items_fail_alone_and_limits_are_one_over_refused() {
    let (store, dir) = (store(), Scratch::new("limits"));
    let node = start(&store, &dir);
    let addr = node.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    let mixed = [
        doc(1),
        "{\"index\":{\"_id\":\"x\"}}\n{\"title\":\"t\",\"tag\":\"a\",\"n\":1,\"ok\":true,\"at\":1,\"extra\":1}\n".into(),
        "{\"index\":{\"_id\":\"y\"}}\n{\"title\":\"t\",\"tag\":\"a\",\"ok\":true,\"at\":1}\n".into(),
        "{\"index\":{\"_id\":\"z\"}}\n{\"title\":\"t\",\"tag\":\"a\",\"n\":\"1\",\"ok\":true,\"at\":1}\n".into(),
        "{\"create\":{\"_id\":\"c\"}}\n{}\n".into(),
        "{\"index\":{}}\n{}\n".into(),
        "{\"index\":{\"_index\":\"missing\",\"_id\":\"m\"}}\n{}\n".into(),
        doc(2),
    ]
    .concat();
    let (status, body) = call(addr, "POST", "/orders/_bulk?refresh=true", &mixed);
    assert_eq!(status, 200);
    assert_eq!(body["errors"], json!(true));
    let statuses: Vec<u64> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            i.as_object().unwrap().values().next().unwrap()["status"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(statuses, vec![201, 400, 400, 400, 400, 400, 404, 201]);
    let (_, all) = call(addr, "POST", "/orders/_search", r#"{"size":0}"#);
    assert_eq!(all["hits"]["total"]["value"], json!(2));

    // One over the item limit is refused with 413; the batches already written
    // are reported item by item in the body (a count cannot be known up front).
    let item = "{\"delete\":{\"_id\":\"1\"}}\n";
    let over_items = item.repeat(MAX_BULK_ITEMS + 1);
    let (status, over) = call(addr, "POST", "/orders/_bulk", &over_items);
    assert_eq!(status, 413);
    assert_eq!(
        over["items"].as_array().unwrap().len(),
        MAX_BULK_ITEMS,
        "{over}"
    );
    let at_limit = item.repeat(MAX_BULK_ITEMS);
    assert_eq!(call(addr, "POST", "/orders/_bulk", &at_limit).0, 200);
    assert_eq!(Limits::default().max_bulk_bytes, MAX_BULK_BYTES);
    // A malformed action line refuses the request, committing nothing.
    let (status, _) = call(
        addr,
        "POST",
        "/orders/_bulk",
        &format!("{}garbage\n", doc(9)),
    );
    assert_eq!(status, 400);
    call(addr, "POST", "/orders/_refresh", "");
    assert_eq!(call(addr, "GET", "/orders/_doc/d9", "").0, 404);
    node.shutdown();
}

#[test]
fn an_overlong_id_fails_alone_and_a_repeat_create_is_refused() {
    let (store, dir) = (store(), Scratch::new("ids"));
    let node = start(&store, &dir);
    let addr = node.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    let long = format!(
        "{{\"index\":{{\"_id\":\"{}\"}}}}\n{{\"title\":\"t\",\"tag\":\"a\",\"n\":1,\"ok\":true,\"at\":1}}\n",
        "i".repeat(257)
    );
    let body = [doc(1), long, doc(2)].concat();
    let (status, out) = call(addr, "POST", "/orders/_bulk", &body);
    assert_eq!(status, 200);
    let statuses: Vec<u64> = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            i.as_object().unwrap().values().next().unwrap()["status"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(statuses, vec![201, 400, 201]);
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 400);
    node.shutdown();
}

#[test]
fn only_loopback_may_be_bound_without_saying_so() {
    let (store, dir) = (store(), Scratch::new("bind"));
    let node = Arc::new(Node::new(Arc::clone(&store), dir.0.clone()));
    assert!(serve(Arc::clone(&node), "0.0.0.0:0", 1, false).is_err());
    assert!(serve(Arc::clone(&node), "not an address", 1, false).is_err());
    let running = serve(node, "0.0.0.0:0", 1, true).unwrap();
    running.shutdown();
}

fn start_with(store: &Arc<MemoryStore>, dir: &Scratch, limits: Limits) -> hsearch_node::Running {
    let node = Arc::new(Node::with_limits(Arc::clone(store), dir.0.clone(), limits).unwrap());
    serve(node, "127.0.0.1:0", 4, false).unwrap()
}

fn small() -> Limits {
    Limits {
        max_document_bytes: 4096,
        max_bulk_bytes: 256 * 1024,
        max_bulk_items: 3000,
        buffer_bytes: 64 * 1024,
        memory_budget_bytes: 8 << 20,
    }
}

/// An index action whose source line is exactly `len` bytes (title padded).
fn sized(id: &str, len: usize) -> String {
    let frame = "{\"title\":\"\",\"tag\":\"a\",\"n\":1,\"ok\":true,\"at\":1}";
    format!(
        "{{\"index\":{{\"_id\":\"{id}\"}}}}\n{{\"title\":\"{}\",\"tag\":\"a\",\"n\":1,\"ok\":true,\"at\":1}}\n",
        "x".repeat(len - frame.len())
    )
}

fn statuses(body: &Value) -> Vec<u64> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            i.as_object().unwrap().values().next().unwrap()["status"]
                .as_u64()
                .unwrap()
        })
        .collect()
}

#[test]
fn a_document_one_byte_over_the_limit_is_refused_by_name_and_nothing_of_it_is_committed() {
    let (store, dir) = (store(), Scratch::new("doclimit"));
    let node = start_with(&store, &dir, small());
    let addr = node.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    let body = [sized("at", 4096), sized("over", 4097), sized("after", 100)].concat();
    let (status, out) = call(addr, "POST", "/orders/_bulk?refresh=true", &body);
    assert_eq!(status, 200, "{out}");
    assert_eq!(statuses(&out), vec![201, 413, 201], "{out}");
    let error = &out["items"][1]["index"]["error"];
    assert_eq!(error["type"], json!("document_too_large_exception"));
    assert!(
        error["reason"]
            .as_str()
            .unwrap()
            .contains("limit of 4096 bytes"),
        "{error}"
    );
    assert_eq!(out["errors"], json!(true));
    assert_eq!(call(addr, "GET", "/orders/_doc/at", "").0, 200);
    assert_eq!(call(addr, "GET", "/orders/_doc/over", "").0, 404);
    assert_eq!(call(addr, "GET", "/orders/_doc/after", "").0, 200);
    node.shutdown();
}

#[test]
fn a_broken_stream_after_the_first_batch_says_how_many_items_were_applied() {
    let (store, dir) = (store(), Scratch::new("partial"));
    let limits = Limits {
        max_bulk_bytes: 4 << 20,
        ..small()
    };
    let node = start_with(&store, &dir, limits);
    let addr = node.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    // 2,500 valid items then a malformed action: the first two batches of 1,000
    // are written, the third is dropped with the request.
    let mut body: String = (0..2500).map(doc).collect();
    body.push_str("garbage\n");
    let (status, out) = call(addr, "POST", "/orders/_bulk?refresh=true", &body);
    assert_eq!(status, 400, "{out}");
    let reason = out["error"]["reason"].as_str().unwrap();
    // A batch ends at 1,000 items or half the buffer, whichever comes first.
    let applied: usize = reason
        .split("; ")
        .nth(1)
        .and_then(|t| t.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{reason}"));
    assert!(
        reason.ends_with("earlier items were already applied"),
        "{reason}"
    );
    assert!((1000..2500).contains(&applied), "{applied}");
    assert_eq!(
        out["items"].as_array().unwrap().len(),
        applied,
        "the applied items are reported"
    );
    assert_eq!(out["errors"], json!(true));
    assert_eq!(
        call(addr, "GET", &format!("/orders/_doc/d{}", applied - 1), "").0,
        200
    );
    assert_eq!(
        call(addr, "GET", &format!("/orders/_doc/d{applied}"), "").0,
        404
    );
    node.shutdown();
}

/// Send a request head promising `length` body bytes, then only `sent` of them.
fn start_stalled(addr: std::net::SocketAddr, length: usize, sent: &str) -> std::net::TcpStream {
    use std::io::Write;
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "POST /orders/_bulk HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {length}\r\n\r\n{sent}"
    )
    .unwrap();
    stream.flush().unwrap();
    stream
}

#[test]
fn concurrent_bulks_beyond_the_memory_budget_are_refused_with_429_and_recover() {
    let (store, dir) = (store(), Scratch::new("gate"));
    let limits = Limits {
        // Exactly one full-size request fits.
        memory_budget_bytes: 3 * 64 * 1024 + 64 * 1024,
        ..small()
    };
    let node = start_with(&store, &dir, limits);
    let addr = node.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    let first = doc(1);
    let total = 200_000;
    let slow = start_stalled(addr, total, &first);
    // Give the worker time to reserve, then a second full-size request is refused.
    let mut refused = None;
    for _ in 0..200 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let (status, body) = call(addr, "POST", "/orders/_bulk", &"\n".repeat(total));
        if status == 429 {
            refused = Some(body);
            break;
        }
    }
    let body = refused.expect("a second large bulk is refused while the first holds the budget");
    assert_eq!(
        body["error"]["type"],
        json!("es_rejected_execution_exception")
    );
    // A small bulk costs little and is still admitted... only when the budget has room,
    // so finish the first request and check everything is released.
    {
        use std::io::{Read, Write};
        let mut slow = slow;
        slow.write_all("\n".repeat(total - first.len()).as_bytes())
            .unwrap();
        let mut raw = String::new();
        slow.read_to_string(&mut raw).unwrap();
        assert!(
            raw.starts_with("HTTP/1.1 200"),
            "{}",
            &raw[..raw.len().min(300)]
        );
    }
    let (status, out) = call(addr, "POST", "/orders/_bulk?refresh=true", &doc(2));
    assert_eq!((status, &out["errors"]), (200, &json!(false)), "{out}");
    node.shutdown();
}

#[test]
fn a_body_over_the_configured_byte_limit_is_refused_before_it_is_read() {
    let (store, dir) = (store(), Scratch::new("bytes"));
    let node = start_with(&store, &dir, small());
    let addr = node.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    let body = "{\"delete\":{\"_id\":\"1\"}}\n".repeat(12_000);
    assert!(body.len() > 256 * 1024);
    assert_eq!(call(addr, "POST", "/orders/_bulk", &body).0, 413);
    node.shutdown();
}
