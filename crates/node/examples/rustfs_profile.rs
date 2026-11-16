//! Measure the node's object-store paths against a real S3-compatible store
//! (the digest-pinned RustFS from `scripts/rustfs.sh`). Four phases, one per
//! argument, each printing plain `key: value` lines that the evidence file quotes.
//!
//!     eval "$(scripts/rustfs.sh up)"
//!     cargo run --release -p hsearch-node --example rustfs_profile -- visibility [trials]
//!     cargo run --release -p hsearch-node --example rustfs_profile -- cold
//!     cargo run --release -p hsearch-node --example rustfs_profile -- delete
//!     cargo run --release -p hsearch-node --example rustfs_profile -- large <MiB>
//!
//! Provisional: RustFS in a container on a shared cloud host, loopback network, one
//! node, single disk. Relative numbers only (AGENTS.md: numbers against an emulator
//! are provisional). Measurement only: nothing here is used by the product.
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant, SystemTime};

use hsearch_index::Wait;
use hsearch_node::{Limits, Node};
use hsearch_object::{
    Budget, Cancellation, Metadata, MetadataStore, Mutation, ObjectBackend, ObjectKey, Payload,
    Precondition, RequestContext, RequestId,
};
use hsearch_s3::{RequestCounts, S3Config, S3Store};
use serde_json::{Value, json};

struct Thread(std::thread::Thread);
impl Wake for Thread {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// The store blocks on its own socket, so its futures are ready at first poll;
/// this only drives them.
fn run<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(Thread(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

fn context(secs: u64) -> RequestContext {
    RequestContext::new(
        Duration::from_secs(secs),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 30,
            response_bytes: 1 << 30,
            attempts: 100_000,
        },
    )
    .unwrap()
}

fn store() -> Arc<S3Store> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let store = S3Store::new(S3Config {
        endpoint: std::env::var("HSEARCH_S3_ENDPOINT")
            .expect("HSEARCH_S3_ENDPOINT (scripts/rustfs.sh up)"),
        bucket: format!("hsp-{nanos}"),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
    .unwrap();
    store.prepare_bucket(&context(60)).expect("prepare bucket");
    Arc::new(store)
}

fn delta(a: RequestCounts, b: RequestCounts) -> String {
    format!(
        "GET {} PUT {} HEAD {} LIST {} DELETE {}",
        b.get - a.get,
        b.put - a.put,
        b.head - a.head,
        b.list - a.list,
        b.delete - a.delete
    )
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    let i = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[i]
}

fn summary(label: &str, mut ms: Vec<f64>) {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{label}: n={} p50 {:.1} ms p90 {:.1} ms p99 {:.1} ms max {:.1} ms",
        ms.len(),
        pct(&ms, 0.5),
        pct(&ms, 0.9),
        pct(&ms, 0.99),
        ms.last().unwrap()
    );
}

fn status_kb(field: &str) -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn scratch(label: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("hsearch-rustfs-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

const MAPPING: &str = r#"{"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"keyword"},"n":{"type":"integer"},"ok":{"type":"boolean"},"at":{"type":"date"}}}}"#;

fn doc_line(i: usize) -> String {
    format!(
        "{{\"index\":{{\"_id\":\"d{i}\"}}}}\n{{\"title\":\"quick fox item{i}\",\"tag\":\"t{}\",\"n\":{i},\"ok\":{},\"at\":{}}}\n",
        i % 3,
        i.is_multiple_of(2),
        1_700_000_000_000_i64 + i as i64
    )
}

fn total(node: &Node<S3Store>) -> u64 {
    node.search("orders", &json!({"query": {"match_all": {}}, "size": 0}))
        .expect("search")["hits"]["total"]["value"]
        .as_u64()
        .unwrap()
}

fn bulk(node: &Node<S3Store>, body: &str, wait: Wait) {
    let (_, out) = node
        .bulk(
            body.as_bytes(),
            Some(body.len() as u64),
            Some("orders"),
            wait,
        )
        .expect("bulk");
    assert_eq!(out["errors"], json!(false), "{out}");
}

/// Write-to-visible latency. For each trial: one document of a batch of `batch`,
/// acknowledged durable, then polled until it can be searched; and the same with
/// `Wait::Searchable` (the call itself is the visibility time).
fn visibility(trials: usize) {
    let store = store();
    let dir = scratch("vis");
    let node = Node::new(Arc::clone(&store), dir.clone());
    node.create("orders", &serde_json::from_str::<Value>(MAPPING).unwrap())
        .unwrap();
    let mut next = 0usize;
    for batch in [1usize, 100] {
        let (mut ack, mut vis, mut searchable) = (vec![], vec![], vec![]);
        let mut puts = (0u64, 0u64);
        let mut others = (0u64, 0u64, 0u64);
        for _ in 0..trials {
            let body: String = (next..next + batch).map(doc_line).collect();
            next += batch;
            let expected = next as u64;
            let (c0, t0) = (store.counts(), Instant::now());
            bulk(&node, &body, Wait::Durable);
            let t_ack = t0.elapsed();
            let c1 = store.counts();
            // A durable write is not searchable until a refresh (M06.07 adds timed modes).
            let t_r = Instant::now();
            node.refresh("orders").expect("refresh");
            let t_vis = t_ack + t_r.elapsed();
            let c2 = store.counts();
            assert_eq!(total(&node), expected, "not searchable after refresh");
            ack.push(t_ack.as_secs_f64() * 1e3);
            vis.push(t_vis.as_secs_f64() * 1e3);
            puts.0 += c1.put - c0.put;
            others.0 += (c1.get - c0.get) + (c1.head - c0.head) + (c1.list - c0.list);
            others.1 +=
                (c2.get - c1.get) + (c2.head - c1.head) + (c2.list - c1.list) + (c2.put - c1.put);
            // Searchable wait, a fresh batch.
            let body: String = (next..next + batch).map(doc_line).collect();
            next += batch;
            let (c3, t3) = (store.counts(), Instant::now());
            bulk(&node, &body, Wait::Searchable);
            searchable.push(t3.elapsed().as_secs_f64() * 1e3);
            let c4 = store.counts();
            assert_eq!(
                total(&node),
                next as u64,
                "Wait::Searchable returned before visible"
            );
            puts.1 += c4.put - c3.put;
            others.2 += (c4.get - c3.get) + (c4.head - c3.head) + (c4.list - c3.list);
        }
        println!("-- batch of {batch} document(s), {trials} trials each");
        summary("durable ack", ack);
        summary(
            "write to searchable (durable ack, then explicit refresh)",
            vis,
        );
        summary("Wait::Searchable call", searchable);
        println!(
            "GET+HEAD+LIST over all trials: durable bulk {}, refresh (GET+HEAD+LIST+PUT) {}, Wait::Searchable bulk {}",
            others.0, others.1, others.2
        );
        println!(
            "PUTs per bulk: durable {:.2}, searchable {:.2}",
            puts.0 as f64 / trials as f64,
            puts.1 as f64 / trials as f64
        );
    }
    let c = store.counts();
    println!(
        "whole phase requests: GET {} PUT {} HEAD {} LIST {} DELETE {}",
        c.get, c.put, c.head, c.list, c.delete
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Requests and time for a node that starts empty on an index that already has data.
fn cold() {
    let store = store();
    let dir = scratch("cold-a");
    let node = Node::new(Arc::clone(&store), dir.clone());
    node.create("orders", &serde_json::from_str::<Value>(MAPPING).unwrap())
        .unwrap();
    for start in (0..10_000).step_by(100) {
        let body: String = (start..start + 100).map(doc_line).collect();
        bulk(&node, &body, Wait::Searchable);
    }
    drop(node);
    let c0 = store.counts();
    let dir2 = scratch("cold-b");
    let t = Instant::now();
    let node = Node::new(Arc::clone(&store), dir2.clone());
    let n = total(&node);
    println!(
        "cold open and first count ({n} docs): {:.1} ms, requests {}",
        t.elapsed().as_secs_f64() * 1e3,
        delta(c0, store.counts())
    );
    for (label, q) in [
        ("term", json!({"term": {"tag": "t1"}})),
        ("range", json!({"range": {"n": {"gte": 10, "lt": 900}}})),
        ("match", json!({"match": {"title": "fox"}})),
    ] {
        let (mut ms, c) = (vec![], store.counts());
        for _ in 0..50 {
            let t = Instant::now();
            node.search("orders", &json!({"query": q, "size": 10}))
                .unwrap();
            ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        println!(
            "warm {label}: requests for 50 searches: {}",
            delta(c, store.counts())
        );
        summary(&format!("warm {label}"), ms);
    }
    let _ = (std::fs::remove_dir_all(dir), std::fs::remove_dir_all(dir2));
}

/// `remove_version` against the real store: removal, repeat removal, a wrong key,
/// a large object, and that neighbours survive.
fn delete() {
    let store = store();
    let ctx = context(60);
    let mutation = |id: &str, key: &str, payload: &[u8]| Mutation {
        request_id: RequestId::new(id.as_bytes()).unwrap(),
        key: ObjectKey::new(key.as_bytes()).unwrap(),
        payload: Payload::new(payload).unwrap(),
        metadata: Metadata::new(b"m").unwrap(),
        precondition: Precondition::Absent,
    };
    let keep = mutation("k", "idx/blob/1/1/0/d.f/0", b"keep");
    let drop = mutation("d", "idx/blob/1/1/0/d.f/1", &vec![7u8; 8 << 20]);
    let kept = run(store.create(&ctx, &keep)).unwrap();
    let t = Instant::now();
    let dropped = run(store.create(&ctx, &drop)).unwrap();
    println!("create 8 MiB: {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    let c0 = store.counts();
    let t = Instant::now();
    let r1 = run(store.remove_version(&ctx, &drop.key, &dropped.object.version));
    println!(
        "remove_version (present): {:?} in {:.1} ms, requests {}",
        r1,
        t.elapsed().as_secs_f64() * 1e3,
        delta(c0, store.counts())
    );
    let c1 = store.counts();
    let r2 = run(store.remove_version(&ctx, &drop.key, &dropped.object.version));
    println!(
        "remove_version (again): {:?}, requests {}",
        r2,
        delta(c1, store.counts())
    );
    let read = run(store.read_range(
        &ctx,
        &drop.key,
        &dropped.object.version,
        hsearch_object::ByteRange::new(0, 4).unwrap(),
    ));
    println!("read removed version: {:?}", read.map(|_| "bytes"));
    let still = run(store.read_range(
        &ctx,
        &keep.key,
        &kept.object.version,
        hsearch_object::ByteRange::new(0, 4).unwrap(),
    ));
    println!("read neighbour: {:?}", still.map(|p| p.as_bytes().to_vec()));
    let head = run(store.head(&ctx, &drop.key));
    println!("head of removed key: {:?}", head.map(|_| "present"));
    // Never-existing version of an existing key.
    let ghost = run(store.remove_version(&ctx, &keep.key, &dropped.object.version));
    println!("remove_version (version of another key): {ghost:?}");
    // Re-create after removal: the key must accept a create again.
    let again = mutation("d2", "idx/blob/1/1/0/d.f/1", b"again");
    println!(
        "create after removal: {:?}",
        run(store.create(&ctx, &again)).map(|_| "ok")
    );
    let c = store.counts();
    println!(
        "whole phase requests: GET {} PUT {} HEAD {} LIST {} DELETE {}",
        c.get, c.put, c.head, c.list, c.delete
    );
}

/// One document with a text field of `mib` MiB through `Node::bulk` on the real
/// store, then a new node on an empty directory replays it from the store.
fn large(mib: f64) {
    // The 200 MiB ceiling counts the whole JSON document, so leave room for its keys.
    assert!(
        (1.0..=200.0).contains(&mib),
        "size must be 1 to 200 MiB: the run holds the input, a copy and the node's working set (about 8x) at once"
    );
    let bytes = (mib * 1_048_576.0) as usize - 1024;
    let store = store();
    let dir = scratch("large-a");
    let limits = Limits {
        max_document_bytes: 200 << 20,
        max_bulk_bytes: 256 << 20,
        max_bulk_items: 100,
        buffer_bytes: 256 << 20,
        memory_budget_bytes: 8192 << 20,
        auto_merge_mb: 0,
    };
    let node = Node::with_limits(Arc::clone(&store), dir.clone(), limits).expect("node");
    node.create("orders", &serde_json::from_str::<Value>(MAPPING).unwrap())
        .unwrap();
    let mut title = String::with_capacity(bytes);
    let mut i = 0u64;
    while title.len() < bytes {
        title.push_str(&format!("w{} ", i % 5000));
        i += 1;
    }
    let body = format!(
        "{{\"index\":{{\"_id\":\"big\"}}}}\n{{\"title\":\"{title}\",\"tag\":\"a\",\"n\":1,\"ok\":true,\"at\":1}}\n"
    );
    // Reset the process high-water mark now that the input exists, so the peak
    // below is the node's own footprint over input-resident memory.
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let baseline = status_kb("VmRSS:");
    let c0 = store.counts();
    let t = Instant::now();
    bulk(&node, &body, Wait::Durable);
    let durable = t.elapsed();
    let c1 = store.counts();
    node.refresh("orders").expect("refresh");
    assert_eq!(total(&node), 1);
    let searchable = t.elapsed();
    let c2 = store.counts();
    let mb = title.len() as f64 / 1_048_576.0;
    println!("document: {mb:.1} MiB");
    println!(
        "durable ack: {:.2} s ({:.1} MiB/s), requests {}",
        durable.as_secs_f64(),
        mb / durable.as_secs_f64(),
        delta(c0, c1)
    );
    println!(
        "searchable: {:.2} s, further requests {}",
        searchable.as_secs_f64(),
        delta(c1, c2)
    );
    println!(
        "peak RSS over baseline: {:.1} MiB ({:.1}x)",
        status_kb("VmHWM:").saturating_sub(baseline) as f64 / 1024.0,
        status_kb("VmHWM:").saturating_sub(baseline) as f64 / 1024.0 / mb
    );
    drop(node);
    drop(body);
    // Replay from the store alone.
    let dir2 = scratch("large-b");
    let c3 = store.counts();
    let t = Instant::now();
    let node = Node::with_limits(Arc::clone(&store), dir2.clone(), limits).expect("node");
    let n = total(&node);
    let hit = node
        .search(
            "orders",
            &json!({"query": {"term": {"tag": "a"}}, "size": 1, "_source": ["n"]}),
        )
        .unwrap()["hits"]["total"]["value"]
        .clone();
    println!(
        "replay on an empty directory: {n} doc(s), term hit {hit}, {:.2} s, requests {}",
        t.elapsed().as_secs_f64(),
        delta(c3, store.counts())
    );
    println!(
        "whole phase requests: {}",
        delta(RequestCounts::default(), store.counts())
    );
    let _ = (std::fs::remove_dir_all(dir), std::fs::remove_dir_all(dir2));
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("visibility") => visibility(args.next().and_then(|v| v.parse().ok()).unwrap_or(100)),
        Some("cold") => cold(),
        Some("delete") => delete(),
        Some("large") => large(args.next().and_then(|v| v.parse().ok()).unwrap_or(200.0)),
        _ => {
            eprintln!("usage: rustfs_profile visibility [trials] | cold | delete | large <MiB>");
            std::process::exit(2);
        }
    }
}
