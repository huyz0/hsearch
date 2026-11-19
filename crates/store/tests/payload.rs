//! A batch too large for a lane record is written as an external payload object before it is
//! committed, and recovery of the log returns the same bytes.
#[path = "support/big.rs"]
mod big;
#[path = "support/payload.rs"]
mod payload_support;
mod support;

use big::Big;
use hsearch_commit::{Dependencies, Outcome, Request};
use hsearch_engine::with_local_engine;
use hsearch_engine::{Kind, Limits, Mutation, Schema, StoredValue, Value};
use hsearch_format::Id;
use hsearch_lane::block_on;
use hsearch_object::{MetadataStore, ObjectBackend, ObjectKey};
use hsearch_store::payload::{ObjectPayloads, PayloadError, PayloadSink};
use hsearch_store::publish::{AnyDependencies, Publisher};
use hsearch_store::rebuild::{RebuildError, rebuild};
use hsearch_store::recover::{Expect, NotReady, ReplayLimits, Spill, recover_with};
use hsearch_store::serve::{ServeError, Serving};
use payload_support::put_payload;
use sha2::{Digest, Sha256};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const MIB: usize = 1 << 20;
const CEILING: usize = 200 * MIB;

fn deps() -> Dependencies {
    Dependencies {
        schema: [1; 32],
        crypto: [1; 32],
        resolved: [1; 32],
    }
}

fn schema(document_bytes: usize) -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword), ("body", Kind::Text)])
        .unwrap()
        .with_limits(Limits::scale().with_document_bytes(document_bytes))
        .unwrap()
}

/// A text body of `len` bytes that is not the same word repeated, so a part cannot stand in for
/// another: blocks of pseudo-random words, each led by its own number.
fn text(len: usize, salt: usize) -> String {
    let mut n = salt;
    let mut block = String::with_capacity(64 * 1024 + 8);
    while block.len() < 64 * 1024 {
        n = n.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        block.push_str(["alpha ", "beta ", "gamma ", "delta ", "kappa "][(n >> 33) % 5]);
    }
    let mut body = String::with_capacity(len + block.len() + 24);
    let mut i = 0usize;
    while body.len() < len {
        body.push_str(&format!("{salt}-{i} "));
        body.push_str(&block);
        i += 1;
    }
    body.truncate(len);
    body
}

fn put<'a>(id: &'a str, version: i64, fields: &'a [(&'a str, Value<'a>)]) -> Mutation<'a> {
    Mutation::Put {
        id,
        version,
        fields,
    }
}

/// The payload sink the writer is given, remembering each reference it staged.
struct Recording {
    inner: ObjectPayloads<Crash>,
    staged: Arc<Mutex<Vec<hsearch_commit::External>>>,
}

impl PayloadSink for Recording {
    fn stage(&self, index: &Id, body: &[u8]) -> Result<hsearch_commit::External, PayloadError> {
        let external = self.inner.stage(index, body)?;
        self.staged.lock().unwrap().push(external);
        Ok(external)
    }

    fn discard(&self, index: &Id, external: &hsearch_commit::External) {
        self.inner.discard(index, external);
    }
}

fn sink(store: &Arc<Crash>) -> (Box<Recording>, Arc<Mutex<Vec<hsearch_commit::External>>>) {
    let staged = Arc::new(Mutex::new(Vec::new()));
    let sink = Recording {
        inner: ObjectPayloads::new(Arc::clone(store), layout(), Duration::from_secs(30)),
        staged: Arc::clone(&staged),
    };
    (Box::new(sink), staged)
}

fn stored_body(engine: &hsearch_engine::Engine<'_>, id: &str) -> Option<String> {
    let snapshot = engine.refresh().unwrap();
    snapshot.get(id, &["body"]).unwrap().map(|hit| {
        let StoredValue::String(s) = &hit.fields[0].1 else {
            panic!("body is a string")
        };
        s.clone()
    })
}

fn recovered_body(
    store: &Arc<Crash>,
    lane: &hsearch_lane::LaneId,
    label: &str,
    ids: &[&str],
    document_bytes: usize,
) -> Result<Vec<Option<String>>, RebuildError> {
    let dir = std::env::temp_dir().join(format!("hsearch-payload-{label}-{}", std::process::id()));
    let schema = schema(document_bytes);
    with_local_engine(&dir, schema.clone(), |engine, _| {
        let limits = ReplayLimits {
            slots: 10_000,
            records: 10_000,
            bytes: 256 << 20,
        };
        rebuild(
            &publisher(store),
            &target(lane, limits, Expect::default()),
            &AnyDependencies,
            &schema,
            engine,
        )?;
        Ok(ids.iter().map(|id| stored_body(engine, id)).collect())
    })
    .unwrap()
}

#[test]
fn large_documents_are_written_as_payload_objects_and_recovered_to_the_same_bytes() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let limit = 16 * MIB;
    let schema = schema(limit);
    // Each side of both bounds: over a record, a chunk exactly, a chunk and a byte, two chunks.
    let sizes = [2 * MIB, 8 * MIB - 100, 8 * MIB, 12 * MIB];
    let bodies: Vec<String> = sizes.iter().enumerate().map(|(i, n)| text(*n, i)).collect();
    let ids = ["a", "b", "c", "d"];
    let dir = std::env::temp_dir().join(format!("hsearch-payload-live-{}", std::process::id()));
    let (writes, staged) = {
        let (sink, staged) = sink(&store);
        let writes = with_local_engine(&dir, schema.clone(), |engine, _| {
            let mut writer = Serving::new(committer, INDEX, 0, deps()).with_payloads(sink);
            let mut writes = Vec::new();
            for (i, body) in bodies.iter().enumerate() {
                let fields = [("tag", Value::String("t")), ("body", Value::String(body))];
                let before = store.writes();
                let written = writer
                    .write(
                        &schema,
                        engine,
                        &format!("r{i}"),
                        &[put(ids[i], 1, &fields)],
                    )
                    .unwrap();
                assert!(written.committed.is_some());
                writes.push(store.writes() - before);
                assert!(!writer.is_broken());
            }
            for (i, body) in bodies.iter().enumerate() {
                assert_eq!(stored_body(engine, ids[i]).as_deref(), Some(body.as_str()));
            }
            writes
        })
        .unwrap();
        (writes, staged)
    };
    let staged = staged.lock().unwrap();
    assert_eq!(staged.len(), 4);
    for (external, body) in staged.iter().zip(&bodies) {
        assert!(
            external.length > body.len() as u64,
            "the batch holds the whole document"
        );
    }
    // Requests scale with bytes: one create, or the parts and the completion of an upload, and
    // the one slot that names it. A batch of a chunk's worth of document is a little over it.
    let expected: Vec<usize> = staged
        .iter()
        .map(|e| {
            let parts = (e.length as usize).div_ceil(8 * MIB);
            1 + parts + usize::from(parts > 1)
        })
        .collect();
    assert_eq!(writes, expected);
    assert_eq!(expected, [2, 2, 4, 4]);
    drop(staged);
    // Recovery fetches the objects and applies the same documents with the writer's engine gone.
    let got = recovered_body(&store, &lane, "match", &ids, limit).unwrap();
    for (got, body) in got.iter().zip(&bodies) {
        assert_eq!(got.as_deref(), Some(body.as_str()));
    }
}

#[test]
fn a_batch_that_fits_a_record_is_not_staged() {
    let store = Crash::new();
    let (_lane, committer) = open_lane(&store);
    let schema = schema(4 * MIB);
    let (sink, staged) = sink(&store);
    let dir = std::env::temp_dir().join(format!("hsearch-payload-small-{}", std::process::id()));
    with_local_engine(&dir, schema.clone(), |engine, _| {
        let mut writer = Serving::new(committer, INDEX, 0, deps()).with_payloads(sink);
        let body = text(MIB / 2, 3);
        let fields = [("tag", Value::String("t")), ("body", Value::String(&body))];
        let before = store.writes();
        writer
            .write(&schema, engine, "r", &[put("a", 1, &fields)])
            .unwrap();
        assert_eq!(store.writes() - before, 1, "one slot and no payload object");
    })
    .unwrap();
    assert!(staged.lock().unwrap().is_empty());
}

#[test]
fn a_batch_of_exactly_a_record_is_inline_and_one_byte_more_is_staged() {
    let schema = schema(4 * MIB);
    let overhead = {
        let fields = [("tag", Value::String("t")), ("body", Value::String(""))];
        hsearch_engine::batch_len("r", &[put("a", 1, &fields)])
    };
    for (extra, staged_objects, writes) in [(0usize, 0usize, 1usize), (1, 1, 2)] {
        let store = Crash::new();
        let (_lane, committer) = open_lane(&store);
        let (sink, staged) = sink(&store);
        let dir = std::env::temp_dir().join(format!(
            "hsearch-payload-edge-{extra}-{}",
            std::process::id()
        ));
        with_local_engine(&dir, schema.clone(), |engine, _| {
            let mut writer = Serving::new(committer, INDEX, 0, deps()).with_payloads(sink);
            let body = text(MIB - overhead + extra, 6);
            let fields = [("tag", Value::String("t")), ("body", Value::String(&body))];
            let before = store.writes();
            writer
                .write(&schema, engine, "r", &[put("a", 1, &fields)])
                .unwrap();
            assert_eq!(store.writes() - before, writes, "extra {extra}");
        })
        .unwrap();
        assert_eq!(
            staged.lock().unwrap().len(),
            staged_objects,
            "extra {extra}"
        );
    }
}

#[test]
fn without_a_payload_sink_a_batch_over_the_record_bound_is_refused_as_before() {
    let store = Crash::new();
    let (_lane, committer) = open_lane(&store);
    let schema = schema(4 * MIB);
    let dir = std::env::temp_dir().join(format!("hsearch-payload-none-{}", std::process::id()));
    with_local_engine(&dir, schema.clone(), |engine, _| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        let body = text(2 * MIB, 4);
        let fields = [("tag", Value::String("t")), ("body", Value::String(&body))];
        let err = writer
            .write(&schema, engine, "r", &[put("a", 1, &fields)])
            .unwrap_err();
        assert_eq!(err, ServeError::Rejected(hsearch_commit::Reject::BadBody));
        assert_eq!(writer.next_seq(), 0);
        assert!(!writer.is_broken());
    })
    .unwrap();
}

#[test]
fn a_store_that_refuses_the_payload_leaves_the_writer_and_the_log_alone() {
    for (label, allow) in [("create", 0), ("part", 1), ("complete", 2)] {
        let store = Crash::new();
        let (lane, committer) = open_lane(&store);
        let limit = 16 * MIB;
        let schema = schema(limit);
        let (sink, _) = sink(&store);
        let dir = std::env::temp_dir().join(format!(
            "hsearch-payload-down-{label}-{}",
            std::process::id()
        ));
        let body = text(12 * MIB, 5);
        with_local_engine(&dir, schema.clone(), |engine, _| {
            let mut writer = Serving::new(committer, INDEX, 0, deps()).with_payloads(sink);
            let fields = [("tag", Value::String("t")), ("body", Value::String(&body))];
            // Two parts and a completion, then the slot: refuse after `allow` of them.
            store.allow.store(allow, Ordering::SeqCst);
            let err = writer
                .write(&schema, engine, "r1", &[put("a", 1, &fields)])
                .unwrap_err();
            assert_eq!(
                err,
                ServeError::Rejected(hsearch_commit::Reject::Store),
                "{label}"
            );
            assert_eq!(writer.next_seq(), 0, "{label}");
            assert!(!writer.is_broken(), "{label}");
            assert_eq!(stored_body(engine, "a"), None, "{label}");
            // The store is back: the same batch is written, and nothing of the failed attempt
            // shows in the log.
            store.allow.store(i64::MAX, Ordering::SeqCst);
            writer
                .write(&schema, engine, "r1", &[put("a", 1, &fields)])
                .unwrap();
        })
        .unwrap();
        let got = recovered_body(&store, &lane, label, &["a"], limit).unwrap();
        assert_eq!(got[0].as_deref(), Some(body.as_str()), "{label}");
    }
}

#[test]
fn a_batch_over_the_document_ceiling_or_empty_is_refused_before_any_request() {
    let store = Crash::new();
    let sink = ObjectPayloads::new(Arc::clone(&store), layout(), Duration::from_secs(30));
    let before = store.writes();
    // Zero pages are never touched: the length is the whole check.
    let over = vec![0u8; CEILING + 1];
    assert_eq!(sink.stage(&INDEX, &over).unwrap_err(), PayloadError::Limit);
    assert_eq!(sink.stage(&INDEX, &[]).unwrap_err(), PayloadError::Limit);
    assert_eq!(store.writes(), before);
}

/// A lane, its committer, and the body `i` of a history at the ceiling.
fn commit_external_history(
    store: &Arc<Crash>,
    big: &Arc<Big>,
    bodies: &[Vec<u8>],
) -> (hsearch_lane::LaneId, Vec<hsearch_commit::External>) {
    let (lane, mut committer) = open_lane(store);
    let sink = ObjectPayloads::new(Arc::clone(big), layout(), Duration::from_secs(60));
    let mut staged = Vec::new();
    for (seq, body) in bodies.iter().enumerate() {
        let external = sink.stage(&INDEX, body).unwrap();
        let request = Request {
            index: INDEX,
            seq: seq as u64,
            body: Vec::new(),
            delete: false,
            deps: deps(),
        };
        match block_on(committer.commit_external(request, external)) {
            Outcome::Committed(_) => {}
            other => panic!("not committed: {other:?}"),
        }
        staged.push(external);
    }
    (lane, staged)
}

fn digests_of(
    big: &Arc<Big>,
    lane: &hsearch_lane::LaneId,
    ceiling: usize,
) -> Result<Vec<[u8; 32]>, NotReady> {
    let publisher = Publisher::new(Arc::clone(big), layout(), 5 << 20, Duration::from_secs(60));
    let limits = ReplayLimits {
        slots: 1000,
        records: 1000,
        bytes: (ceiling * 8) as u64,
    };
    let mut seen = Vec::new();
    recover_with(
        &publisher,
        &target(lane, limits, Expect::default()),
        &AnyDependencies,
        &Spill::Refuse,
        |record| {
            seen.push(Sha256::digest(record.body).into());
            Ok(())
        },
    )?;
    Ok(seen)
}

#[test]
fn a_history_up_to_the_document_ceiling_recovers_the_same_bytes() {
    let store = Crash::new();
    let big = Big::new(&store);
    let sizes = [
        2 * MIB,
        8 * MIB,
        8 * MIB + 1,
        24 * MIB + 5,
        100 * MIB,
        CEILING,
    ];
    let bodies: Vec<Vec<u8>> = sizes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let mut v = text(*n, 10 + i).into_bytes();
            v[0] = b'A' + i as u8;
            v
        })
        .collect();
    let expected: Vec<[u8; 32]> = bodies.iter().map(|b| Sha256::digest(b).into()).collect();
    let (lane, staged) = commit_external_history(&store, &big, &bodies);
    // Every object is the length of its body and no part was larger than a chunk.
    assert_eq!(
        staged.iter().map(|e| e.length as usize).collect::<Vec<_>>(),
        sizes
    );
    assert_eq!(big.largest_part.load(Ordering::SeqCst), 8 * MIB as u64);
    // One create for a body that fits a chunk, a begin, a request per 8 MiB and a completion for
    // the rest: requests scale with bytes.
    let parts: usize = sizes.iter().map(|n| n.div_ceil(8 * MIB)).sum();
    let multipart = sizes.iter().filter(|n| **n > 8 * MIB).count();
    assert_eq!(
        big.payload_requests() as usize,
        parts + 2 * multipart,
        "creates and parts, plus a begin and a completion for each upload"
    );
    assert_eq!(digests_of(&big, &lane, CEILING).unwrap(), expected);

    // A chunk that is gone, one byte that changed, and a short object each refuse recovery at
    // the sequence that names them, and not before it.
    let key_of = |seq: usize| {
        ObjectKey::new(layout().payload(&INDEX, &staged[seq].object).as_bytes())
            .unwrap()
            .as_bytes()
            .to_vec()
    };
    let mut keep = big.objects.lock().unwrap().remove(&key_of(4)).unwrap();
    assert_eq!(digests_of(&big, &lane, CEILING), Err(NotReady::Payload(4)));
    keep[60 * MIB] ^= 1;
    big.objects.lock().unwrap().insert(key_of(4), keep.clone());
    assert_eq!(digests_of(&big, &lane, CEILING), Err(NotReady::Payload(4)));
    keep[60 * MIB] ^= 1;
    keep.pop();
    big.objects.lock().unwrap().insert(key_of(4), keep);
    assert_eq!(digests_of(&big, &lane, CEILING), Err(NotReady::Payload(4)));
}

#[test]
fn a_lost_or_altered_payload_refuses_recovery_at_its_sequence() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let limit = 16 * MIB;
    let schema = schema(limit);
    let (sink, staged) = sink(&store);
    let dir = std::env::temp_dir().join(format!("hsearch-payload-lost-{}", std::process::id()));
    let bodies = [text(3 * MIB, 20), text(3 * MIB, 21), text(3 * MIB, 22)];
    with_local_engine(&dir, schema.clone(), |engine, _| {
        let mut writer = Serving::new(committer, INDEX, 0, deps()).with_payloads(sink);
        for (i, body) in bodies.iter().enumerate() {
            let fields = [("tag", Value::String("t")), ("body", Value::String(body))];
            let id = ["a", "b", "c"][i];
            writer
                .write(&schema, engine, &format!("r{i}"), &[put(id, 1, &fields)])
                .unwrap();
        }
    })
    .unwrap();
    let staged = staged.lock().unwrap().clone();
    let key = ObjectKey::new(layout().payload(&INDEX, &staged[1].object).as_bytes()).unwrap();
    let info = block_on(store.head(&ctx(), &key)).unwrap();
    let original = read_all(&store, std::str::from_utf8(key.as_bytes()).unwrap());
    // Lost: the first document recovers and the log is not ready at the second.
    block_on(store.remove_version(&ctx(), &key, &info.version)).unwrap();
    let err = recovered_body(&store, &lane, "gone", &["a"], limit).unwrap_err();
    assert!(
        matches!(err, RebuildError::NotReady(NotReady::Payload(1))),
        "{err:?}"
    );
    // Altered: the same length with one byte changed.
    let mut altered = original.clone();
    altered[1000] ^= 1;
    put_payload(&store, &INDEX, &staged[1].object, &altered);
    let err = recovered_body(&store, &lane, "bad", &["a"], limit).unwrap_err();
    assert!(
        matches!(err, RebuildError::NotReady(NotReady::Payload(1))),
        "{err:?}"
    );
}

#[test]
fn identical_documents_get_distinct_payload_objects() {
    // Each payload has its own name, so two documents of identical bytes are two objects.
    let store = Crash::new();
    let big = Big::new(&store);
    let sink = ObjectPayloads::new(Arc::clone(&big), layout(), Duration::from_secs(60));
    let body = text(3 * MIB, 30).into_bytes();
    let a = sink.stage(&INDEX, &body).unwrap();
    let b = sink.stage(&INDEX, &body).unwrap();
    assert_eq!(a.digest, b.digest);
    assert_ne!(a.object, b.object);
    assert_eq!(big.keys().len(), 2);
}

#[test]
fn a_payload_whose_record_was_refused_is_removed_and_a_lost_create_reply_is_settled() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let limit = 16 * MIB;
    let schema = schema(limit);
    let (sink, staged) = sink(&store);
    let dir = std::env::temp_dir().join(format!("hsearch-payload-orphan-{}", std::process::id()));
    let body = text(3 * MIB, 40);
    with_local_engine(&dir, schema.clone(), |engine, _| {
        let mut writer = Serving::new(committer, INDEX, 0, deps()).with_payloads(sink);
        let fields = [("tag", Value::String("t")), ("body", Value::String(&body))];
        // The object is created, then the slot is refused.
        store.allow.store(1, Ordering::SeqCst);
        let err = writer
            .write(&schema, engine, "r1", &[put("a", 1, &fields)])
            .unwrap_err();
        assert_eq!(err, ServeError::Rejected(hsearch_commit::Reject::Store));
        let object = staged.lock().unwrap()[0].object;
        let key = ObjectKey::new(layout().payload(&INDEX, &object).as_bytes()).unwrap();
        assert!(block_on(store.head(&ctx(), &key)).is_err(), "removed");
        // The create's reply is lost after it landed: resolved by identity, the write completes.
        store.allow.store(i64::MAX, Ordering::SeqCst);
        store.lose.store(store.writes() as i64, Ordering::SeqCst);
        writer
            .write(&schema, engine, "r1", &[put("a", 1, &fields)])
            .unwrap();
        assert!(!writer.is_broken());
    })
    .unwrap();
    let got = recovered_body(&store, &lane, "orphan", &["a"], limit).unwrap();
    assert_eq!(got[0].as_deref(), Some(body.as_str()));
}
