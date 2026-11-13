//! The node against a real S3-compatible store: 100,000 documents through plain
//! HTTP, a restart onto an empty data directory, and the same answers after.
//! Needs `HSEARCH_S3_ENDPOINT` and credentials (see `scripts/rustfs.sh`); without
//! them the test prints SKIPPED unless `HSEARCH_S3_REQUIRED=1`.
mod support;

use hsearch_node::{MAX_BULK_ITEMS, Node, serve};
use hsearch_s3::{S3Config, S3Store};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use support::*;

const DOCUMENTS: usize = 100_000;

fn store() -> Option<Arc<S3Store>> {
    let Ok(endpoint) = std::env::var("HSEARCH_S3_ENDPOINT") else {
        assert!(
            std::env::var("HSEARCH_S3_REQUIRED").as_deref() != Ok("1"),
            "HSEARCH_S3_REQUIRED=1 but HSEARCH_S3_ENDPOINT is unset"
        );
        eprintln!(
            "SKIPPED: HSEARCH_S3_ENDPOINT is not set; nothing was checked against a real store"
        );
        return None;
    };
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let store = S3Store::new(S3Config {
        endpoint,
        bucket: format!("hsn-{nanos}"),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
    .unwrap();
    let context = hsearch_object::RequestContext::new(
        Duration::from_secs(60),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 64,
        },
    )
    .unwrap();
    store.prepare_bucket(&context).expect("prepare bucket");
    Some(Arc::new(store))
}

fn search(addr: std::net::SocketAddr, body: Value) -> Value {
    let (status, out) = call(addr, "POST", "/orders/_search", &body.to_string());
    assert_eq!(status, 200, "{out}");
    out
}

/// A fixed query set and its answers: counts and the first ids of a sorted page.
fn answers(addr: std::net::SocketAddr) -> Vec<Value> {
    let total =
        |q: Value| search(addr, json!({"query": q, "size": 0}))["hits"]["total"]["value"].clone();
    let page = search(
        addr,
        json!({"query": {"term": {"tag": "t2"}}, "sort": [{"n": "desc"}], "size": 5, "_source": ["n"]}),
    );
    vec![
        total(json!({"match_all": {}})),
        total(json!({"term": {"tag": "t0"}})),
        total(json!({"range": {"n": {"gte": 1000, "lt": 51000}}})),
        total(json!({"bool": {"filter": [{"term": {"ok": true}}, {"term": {"tag": "t1"}}]}})),
        total(json!({"match": {"title": "item99999"}})),
        page["hits"]["hits"].clone(),
    ]
}

#[test]
fn a_hundred_thousand_documents_over_http_survive_a_restart() {
    let Some(store) = store() else {
        return;
    };
    let (first, second) = (Scratch::new("rf1"), Scratch::new("rf2"));
    let node = Arc::new(Node::new(Arc::clone(&store), first.0.clone()));
    let running = serve(node, "127.0.0.1:0", 4, false).unwrap();
    let addr = running.addr;
    assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    let started = Instant::now();
    for start in (0..DOCUMENTS).step_by(MAX_BULK_ITEMS) {
        let body: String = (start..(start + MAX_BULK_ITEMS).min(DOCUMENTS))
            .map(|i| {
                format!(
                    "{{\"index\":{{\"_id\":\"d{i}\"}}}}\n{{\"title\":\"quick fox item{i}\",\"tag\":\"t{}\",\"n\":{i},\"ok\":{},\"at\":{}}}\n",
                    i % 3,
                    i.is_multiple_of(2),
                    1_700_000_000_000_i64 + i as i64
                )
            })
            .collect();
        let (status, out) = call(addr, "POST", "/orders/_bulk?refresh=true", &body);
        assert_eq!((status, &out["errors"]), (200, &json!(false)));
    }
    eprintln!(
        "bulk of {DOCUMENTS} over HTTP: {:?} (provisional, debug build)",
        started.elapsed()
    );
    let live = answers(addr);
    assert_eq!(live[0], json!(DOCUMENTS));
    assert_eq!(live[1], json!(DOCUMENTS.div_ceil(3)));
    assert_eq!(live[2], json!(50_000));
    assert_eq!(live[4], json!(1));
    let counts = store.counts();
    assert_eq!(counts.list, 0, "no LIST on any path");
    running.shutdown();

    // A new process in spirit: a new node, an empty data directory, the same store.
    drop(first);
    let started = Instant::now();
    let node = Arc::new(Node::new(Arc::clone(&store), second.0.clone()));
    let running = serve(node, "127.0.0.1:0", 4, false).unwrap();
    let addr = running.addr;
    assert_eq!(answers(addr), live);
    eprintln!("restart and first answers: {:?}", started.elapsed());
    assert_eq!(store.counts().list, 0);
    // The restarted node keeps writing.
    let (status, out) = call(
        addr,
        "POST",
        "/orders/_bulk?refresh=true",
        &doc(DOCUMENTS + 1),
    );
    assert_eq!((status, &out["errors"]), (200, &json!(false)));
    let (_, total) = call(addr, "POST", "/orders/_search", r#"{"size":0}"#);
    assert_eq!(total["hits"]["total"]["value"], json!(DOCUMENTS + 1));
    running.shutdown();
}

/// Requests to the store caused by one HTTP call, by call type, on a real store.
#[test]
fn requests_per_http_call_by_type_never_list() {
    let Some(store) = store() else {
        return;
    };
    let dir = Scratch::new("rfc");
    let node = Arc::new(Node::new(Arc::clone(&store), dir.0.clone()));
    let running = serve(node, "127.0.0.1:0", 4, false).unwrap();
    let addr = running.addr;
    let measure = |label: &str, f: &dyn Fn()| -> [u64; 5] {
        let before = store.counts();
        f();
        let after = store.counts();
        let d = [
            after.get - before.get,
            after.put - before.put,
            after.head - before.head,
            after.list - before.list,
            after.delete - before.delete,
        ];
        eprintln!(
            "requests {label}: GET {} PUT {} HEAD {} LIST {} DELETE {}",
            d[0], d[1], d[2], d[3], d[4]
        );
        d
    };
    let bulk = |range: std::ops::Range<usize>, query: &str| {
        let body: String = range.map(doc).collect();
        let (status, out) = call(addr, "POST", &format!("/orders/_bulk{query}"), &body);
        assert_eq!((status, &out["errors"]), (200, &json!(false)));
    };
    let create = measure("create index", &|| {
        assert_eq!(call(addr, "PUT", "/orders", MAPPING).0, 200);
    });
    assert_eq!(create[3], 0);
    let durable = measure("bulk of 1000, durable", &|| bulk(0..1000, ""));
    assert_eq!(durable, [0, 1, 0, 0, 0], "one PUT, nothing else");
    let searchable = measure("bulk of 1000, refresh=wait_for", &|| {
        bulk(1000..2000, "?refresh=wait_for");
    });
    assert_eq!(searchable, [0, 1, 0, 0, 0], "visibility adds no request");
    let refresh = measure("refresh", &|| {
        assert_eq!(call(addr, "POST", "/orders/_refresh", "").0, 200);
    });
    assert_eq!(refresh[3], 0);
    let queries = [
        ("match_all", json!({"match_all": {}})),
        ("term", json!({"term": {"tag": "t1"}})),
        ("range", json!({"range": {"n": {"gte": 10, "lt": 900}}})),
        ("match", json!({"match": {"title": "fox"}})),
        (
            "bool",
            json!({"bool": {"filter": [{"term": {"ok": true}}, {"term": {"tag": "t0"}}]}}),
        ),
    ];
    for (label, query) in &queries {
        let d = measure(&format!("search {label}"), &|| {
            let (status, _) = call(
                addr,
                "POST",
                "/orders/_search",
                &json!({"query": query, "size": 10}).to_string(),
            );
            assert_eq!(status, 200);
        });
        assert_eq!(d, [0; 5], "a search on an open index reads no object");
    }
    let get = measure("get by id", &|| {
        assert_eq!(call(addr, "GET", "/orders/_doc/d5", "").0, 200);
    });
    assert_eq!(get, [0; 5]);
    assert_eq!(store.counts().list, 0);
    running.shutdown();
}
