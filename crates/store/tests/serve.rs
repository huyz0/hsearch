//! A serving writer logs only batches the engine accepted, and what it holds live is what a
//! recovery of its log holds.
#[path = "support/history.rs"]
mod history;
mod support;

use history::*;
use hsearch_commit::Dependencies;
use hsearch_engine::{Error as EngineError, Mutation, StoredValue, Value, with_local_engine};
use hsearch_store::publish::AnyDependencies;
use hsearch_store::rebuild::rebuild;
use hsearch_store::recover::Expect;
use hsearch_store::serve::{ChainBatch, ChainStop, ServeError, Serving};
use std::sync::atomic::Ordering;
use support::*;

fn deps() -> Dependencies {
    Dependencies {
        schema: [1; 32],
        crypto: [1; 32],
        resolved: [1; 32],
    }
}

fn put<'a>(id: &'a str, version: i64, tag: &'a str) -> Mutation<'a> {
    Mutation::Put {
        id,
        version,
        fields: Box::leak(Box::new([("tag", Value::String(tag))])),
    }
}

fn tag_of(engine: &hsearch_engine::Engine<'_>, id: &str) -> Option<(i64, String)> {
    let snapshot = engine.refresh().unwrap();
    snapshot.get(id, &["tag"]).unwrap().map(|hit| {
        let StoredValue::String(tag) = &hit.fields[0].1 else {
            panic!("tag is a string")
        };
        (hit.version, tag.clone())
    })
}

/// Recover `store`'s log into a fresh engine and return the version and tag of each of `ids`
/// with the number of batches applied.
fn recovered(
    store: &std::sync::Arc<Crash>,
    lane: &hsearch_lane::LaneId,
    label: &str,
    ids: &[&str],
) -> (u64, Vec<Option<(i64, String)>>) {
    let dir = std::env::temp_dir().join(format!("hsearch-serve-{label}-{}", std::process::id()));
    with_local_engine(&dir, schema(), |engine, _| {
        let rebuilt = rebuild(
            &publisher(store),
            &target(lane, big(), Expect::default()),
            &AnyDependencies,
            &schema(),
            engine,
        )
        .unwrap();
        (
            rebuilt.recovered.applied,
            ids.iter().map(|id| tag_of(engine, id)).collect(),
        )
    })
    .unwrap()
}

#[test]
fn only_accepted_batches_are_logged_and_recovery_matches_the_live_engine() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let schema = schema();
    let dir = std::env::temp_dir().join(format!("hsearch-serve-live-{}", std::process::id()));
    let live = with_local_engine(&dir, schema.clone(), |engine, _| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        let ok = writer
            .write(&schema, engine, "r1", &[put("a", 1, "x"), put("b", 1, "y")])
            .unwrap();
        assert_eq!(ok.receipts.len(), 2);
        assert!(ok.committed.is_some());
        // A stale version is refused with nothing written, and the sequence is not spent.
        assert_eq!(
            writer
                .write(&schema, engine, "r2", &[put("a", 1, "stale")])
                .unwrap_err(),
            ServeError::Refused(EngineError::Conflict)
        );
        assert_eq!(writer.next_seq(), 1);
        // A batch that is refused partway (its second mutation is stale) writes none of it.
        assert_eq!(
            writer
                .write(
                    &schema,
                    engine,
                    "r3",
                    &[put("c", 5, "n"), put("b", 1, "stale")]
                )
                .unwrap_err(),
            ServeError::Refused(EngineError::Conflict)
        );
        assert_eq!(tag_of(engine, "c"), None);
        writer
            .write(&schema, engine, "r4", &[put("a", 2, "z")])
            .unwrap();
        // The same request again is recognised, not written twice.
        let again = writer
            .write(&schema, engine, "r4", &[put("a", 2, "z")])
            .unwrap();
        assert!(again.committed.is_none());
        assert_eq!(again.receipts.len(), 1);
        assert_eq!(writer.next_seq(), 2);
        assert!(!writer.is_broken());
        (
            tag_of(engine, "a"),
            tag_of(engine, "b"),
            tag_of(engine, "c"),
        )
    })
    .unwrap();
    assert_eq!(live.0, Some((2, "z".into())));
    // The log holds the two accepted batches and nothing else: a refused one would stop recovery.
    let (applied, got) = recovered(&store, &lane, "match", &["a", "b", "c"]);
    assert_eq!(applied, 2);
    assert_eq!(got, vec![live.0, live.1, live.2]);
}

#[test]
fn a_store_that_refuses_the_record_leaves_the_engine_and_the_sequence_alone() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let schema = schema();
    let dir = std::env::temp_dir().join(format!("hsearch-serve-down-{}", std::process::id()));
    with_local_engine(&dir, schema.clone(), |engine, _| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        writer
            .write(&schema, engine, "r1", &[put("a", 1, "x")])
            .unwrap();
        store.allow.store(0, Ordering::SeqCst);
        let err = writer
            .write(&schema, engine, "r2", &[put("a", 2, "y")])
            .unwrap_err();
        assert!(matches!(err, ServeError::Rejected(_)), "{err:?}");
        assert_eq!(tag_of(engine, "a"), Some((1, "x".into())));
        assert_eq!(writer.next_seq(), 1);
        // The store comes back and the same batch is written at the same sequence.
        store.allow.store(i64::MAX, Ordering::SeqCst);
        assert!(!writer.is_broken());
        writer
            .write(&schema, engine, "r2", &[put("a", 2, "y")])
            .unwrap();
        assert_eq!(writer.next_seq(), 2);
    })
    .unwrap();
    let (applied, got) = recovered(&store, &lane, "down", &["a"]);
    assert_eq!(applied, 2);
    assert_eq!(got, vec![Some((2, "y".into()))]);
}

#[test]
fn a_lost_reply_breaks_the_writer_and_recovery_settles_the_batch() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let schema = schema();
    let dir = std::env::temp_dir().join(format!("hsearch-serve-lost-{}", std::process::id()));
    with_local_engine(&dir, schema.clone(), |engine, _| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        writer
            .write(&schema, engine, "r1", &[put("a", 1, "x")])
            .unwrap();
        // The next write commits but its reply is lost, and the store cannot resolve it.
        store.fail_resolve.store(true, Ordering::SeqCst);
        store.lose.store(store.writes() as i64, Ordering::SeqCst);
        let err = writer
            .write(&schema, engine, "r2", &[put("a", 2, "y")])
            .unwrap_err();
        assert!(matches!(err, ServeError::Unknown { .. }), "{err:?}");
        // The live engine did not take it, and the writer refuses everything until recovered.
        assert_eq!(tag_of(engine, "a"), Some((1, "x".into())));
        assert!(writer.is_broken());
        assert_eq!(
            writer
                .write(&schema, engine, "r3", &[put("b", 1, "q")])
                .unwrap_err(),
            ServeError::Broken
        );
    })
    .unwrap();
    // The log has the batch whose reply was lost, so a recovery serves it.
    let (applied, got) = recovered(&store, &lane, "lost", &["a", "b"]);
    assert_eq!(applied, 2);
    assert_eq!(got, vec![Some((2, "y".into())), None]);
}

/// A dependency view that evolves the engine's schema while the record is being committed, the
/// one window in which the engine changes between admit and apply.
struct Evolving<'a, 'e> {
    engine: &'a hsearch_engine::Engine<'e>,
    next: hsearch_engine::Schema,
    fired: std::cell::Cell<bool>,
}

impl hsearch_commit::DependencyView for Evolving<'_, '_> {
    fn check(&self, _: &hsearch_format::Id, _: &Dependencies) -> Result<(), hsearch_commit::Stale> {
        if !self.fired.replace(true) {
            self.engine.evolve_schema(self.next.clone()).unwrap();
        }
        Ok(())
    }
}

#[test]
fn a_batch_the_engine_refuses_after_its_commit_breaks_the_writer_and_recovery_serves_it() {
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    let schema = schema();
    let evolved = schema
        .evolve(2, &[("extra", hsearch_engine::Kind::Keyword)])
        .unwrap();
    let dir = std::env::temp_dir().join(format!("hsearch-serve-post-{}", std::process::id()));
    with_local_engine(&dir, schema.clone(), |engine, _| {
        let (lane_writer, _) = hsearch_lane::block_on(hsearch_lane::LaneWriter::open_with_head(
            std::sync::Arc::clone(&store),
            layout(),
            lane,
            |_| Ok(()),
        ))
        .unwrap();
        let committer = hsearch_commit::Committer::new(
            lane_writer,
            Evolving {
                engine,
                next: evolved,
                fired: std::cell::Cell::new(false),
            },
            hsearch_lane::batch::BatchPolicy {
                max_records: 1,
                max_bytes: 4 << 20,
                max_wait: std::time::Duration::from_millis(1),
            },
        )
        .unwrap();
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        // The schema moves between admit and apply: the batch is committed, the apply refuses it.
        let err = writer
            .write(&schema, engine, "r1", &[put("a", 1, "x")])
            .unwrap_err();
        assert_eq!(err, ServeError::Broken);
        assert!(writer.is_broken());
        assert_eq!(tag_of(engine, "a"), None);
        assert_eq!(
            writer
                .write(&schema, engine, "r2", &[put("b", 1, "y")])
                .unwrap_err(),
            ServeError::Broken
        );
    })
    .unwrap();
    // The committed batch is in the log and a recovery serves it.
    let (applied, got) = recovered(&store, &lane, "post", &["a", "b"]);
    assert_eq!(applied, 1);
    assert_eq!(got, vec![Some((1, "x".into())), None]);
}

#[test]
fn a_chain_logs_the_batches_before_the_first_the_engine_refuses_and_none_after_it() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let schema = schema();
    let dir = std::env::temp_dir().join(format!("hsearch-serve-chain-{}", std::process::id()));
    let before = store.writes();
    with_local_engine(&dir, schema.clone(), |engine, _| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        let first = [put("a", 1, "x")];
        // Not above the version the chain's first batch gives "a": refused.
        let stale = [put("a", 1, "y")];
        // Fresh on its own, but it comes after the refused one.
        let later = [put("b", 1, "z")];
        let chained = writer.write_chain(
            &schema,
            engine,
            &[
                ChainBatch {
                    request: "c1",
                    mutations: &first,
                },
                ChainBatch {
                    request: "c2",
                    mutations: &stale,
                },
                ChainBatch {
                    request: "c3",
                    mutations: &later,
                },
            ],
        );
        assert_eq!(chained.receipts.len(), 1);
        assert_eq!(
            chained.stop,
            Some(ChainStop::Refused(EngineError::Conflict))
        );
        assert_eq!(writer.next_seq(), 1);
        assert_eq!(tag_of(engine, "b"), None);
    })
    .unwrap();
    // One record is one write, and a recovery finds exactly the batch that was logged.
    assert_eq!(store.writes() - before, 1);
    let (applied, found) = recovered(&store, &lane, "chain", &["a", "b"]);
    assert_eq!(applied, 1);
    assert_eq!(found[0], Some((1, "x".into())));
    assert_eq!(found[1], None);
}
