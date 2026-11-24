//! The serving writer publishes readers through the barrier, and a refresh that fails at any step
//! leaves the reader that was serving.
#[path = "support/history.rs"]
mod history;
mod support;

use std::cell::Cell;
use std::sync::Arc;

use history::*;
use hsearch_commit::Dependencies;
use hsearch_engine::{Engine, Mutation, StoredValue, Value, with_local_engine};
use hsearch_index::barrier::{Gen, Refuse};
use hsearch_store::serve::{RefreshError, Serving};
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

fn seen(reader: &Gen<hsearch_engine::Snapshot>, id: &str) -> Option<String> {
    reader.reader.get(id, &["tag"]).unwrap().map(|hit| {
        let StoredValue::String(tag) = &hit.fields[0].1 else {
            panic!("tag is a string")
        };
        tag.clone()
    })
}

fn write<S, D>(w: &mut Serving<hsearch_commit::Committer<S, D>>, engine: &Engine<'_>, n: u32)
where
    S: hsearch_object::MetadataStore,
    D: hsearch_commit::DependencyView,
{
    let id = format!("d{n}");
    let tag = format!("t{n}");
    w.write(&schema(), engine, &format!("r{n}"), &[put(&id, 1, &tag)])
        .unwrap();
}

#[test]
fn a_reader_covers_exactly_the_batches_applied_when_it_was_published() {
    let store = Crash::new();
    let (_, committer) = open_lane(&store);
    let dir = std::env::temp_dir().join(format!("hsearch-readers-a-{}", std::process::id()));
    with_local_engine(&dir, schema(), |engine, _| {
        let mut w = Serving::new(committer, INDEX, 0, deps());
        assert!(w.reader().is_none());
        assert!(!w.refresh_due());
        assert!(w.refresh(engine).unwrap().is_none());
        write(&mut w, engine, 1);
        assert!(w.refresh_due());
        let first = w.refresh(engine).unwrap().unwrap();
        assert_eq!((first.covers, first.generation), (1, 1));
        write(&mut w, engine, 2);
        // The held reader is frozen; the writer is ahead of it until the next refresh.
        assert_eq!(seen(&first, "d2"), None);
        let second = w.refresh(engine).unwrap().unwrap();
        assert_eq!((second.covers, second.generation), (2, 2));
        assert_eq!(seen(&second, "d1").as_deref(), Some("t1"));
        assert_eq!(seen(&second, "d2").as_deref(), Some("t2"));
        // Nothing new applied: no refresh is due and the same generation serves.
        assert!(!w.refresh_due());
        let again = w.refresh(engine).unwrap().unwrap();
        assert_eq!(again.generation, 2);
    })
    .unwrap();
}

#[test]
fn a_refresh_the_engine_cannot_open_leaves_the_serving_reader_and_the_next_one_succeeds() {
    let store = Crash::new();
    let (_, committer) = open_lane(&store);
    let dir = std::env::temp_dir().join(format!("hsearch-readers-b-{}", std::process::id()));
    with_local_engine(&dir, schema(), |engine, _| {
        let mut w = Serving::new(committer, INDEX, 0, deps());
        write(&mut w, engine, 1);
        w.refresh(engine).unwrap();
        write(&mut w, engine, 2);
        // The engine allows four open snapshots; the published one is a fourth after three held.
        let held: Vec<_> = (0..3).map(|_| engine.refresh().unwrap()).collect();
        let failed = w.refresh(engine);
        assert!(
            matches!(failed, Err(RefreshError::Engine(_))),
            "{:?}",
            failed.as_ref().err()
        );
        let serving = w.reader().unwrap();
        assert_eq!((serving.generation, serving.covers), (1, 1));
        assert_eq!(seen(&serving, "d1").as_deref(), Some("t1"));
        assert_eq!(seen(&serving, "d2"), None);
        drop(held);
        let next = w.refresh(engine).unwrap().unwrap();
        assert_eq!((next.generation, next.covers), (2, 2));
        assert_eq!(seen(&next, "d2").as_deref(), Some("t2"));
    })
    .unwrap();
}

#[test]
fn a_refresh_refused_because_queries_hold_old_generations_leaves_the_reader_and_retries() {
    let store = Crash::new();
    let (_, committer) = open_lane(&store);
    let dir = std::env::temp_dir().join(format!("hsearch-readers-c-{}", std::process::id()));
    with_local_engine(&dir, schema(), |engine, _| {
        let mut w = Serving::new(committer, INDEX, 0, deps());
        write(&mut w, engine, 1);
        let held_one = w.refresh(engine).unwrap().unwrap();
        write(&mut w, engine, 2);
        let held_two = w.refresh(engine).unwrap().unwrap();
        write(&mut w, engine, 3);
        let refused = w.refresh(engine);
        assert!(
            matches!(refused, Err(RefreshError::Refused(Refuse::Pinned))),
            "{:?}",
            refused.as_ref().err()
        );
        let serving = w.reader().unwrap();
        assert_eq!(serving.generation, 2);
        assert_eq!(seen(&serving, "d3"), None);
        drop((held_one, held_two, serving));
        let next = w.refresh(engine).unwrap().unwrap();
        assert_eq!((next.generation, next.covers), (3, 3));
        assert_eq!(seen(&next, "d3").as_deref(), Some("t3"));
    })
    .unwrap();
}

/// A dependency view that evolves the engine's schema while the third record is committed: the
/// batch is durable, the engine refuses it, and the writer is broken.
struct Evolving<'a, 'e> {
    engine: &'a Engine<'e>,
    next: hsearch_engine::Schema,
    calls: Cell<u32>,
}

impl hsearch_commit::DependencyView for Evolving<'_, '_> {
    fn check(&self, _: &hsearch_format::Id, _: &Dependencies) -> Result<(), hsearch_commit::Stale> {
        self.calls.set(self.calls.get() + 1);
        if self.calls.get() == 3 {
            self.engine.evolve_schema(self.next.clone()).unwrap();
        }
        Ok(())
    }
}

#[test]
fn a_barrier_that_cannot_be_cut_publishes_nothing_and_the_reader_keeps_serving() {
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    let evolved = schema()
        .evolve(2, &[("extra", hsearch_engine::Kind::Keyword)])
        .unwrap();
    let dir = std::env::temp_dir().join(format!("hsearch-readers-d-{}", std::process::id()));
    with_local_engine(&dir, schema(), |engine, _| {
        let (lane_writer, _) = hsearch_lane::block_on(hsearch_lane::LaneWriter::open_with_head(
            Arc::clone(&store),
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
                calls: Cell::new(0),
            },
            hsearch_lane::batch::BatchPolicy {
                max_records: 1,
                max_bytes: 4 << 20,
                max_wait: std::time::Duration::from_millis(1),
            },
        )
        .unwrap();
        let mut w = Serving::new(committer, INDEX, 0, deps());
        write(&mut w, engine, 1);
        w.refresh(engine).unwrap();
        write(&mut w, engine, 2);
        assert!(
            w.write(&schema(), engine, "r3", &[put("d3", 1, "t3")])
                .is_err()
        );
        assert!(w.is_broken());
        // Batch 2 is applied and due, but the cursor failed: no cut, nothing published.
        assert!(w.refresh_due());
        let failed = w.refresh(engine);
        assert!(
            matches!(failed, Err(RefreshError::Cut)),
            "{:?}",
            failed.as_ref().err()
        );
        let serving = w.reader().unwrap();
        assert_eq!((serving.generation, serving.covers), (1, 1));
        assert_eq!(seen(&serving, "d1").as_deref(), Some("t1"));
    })
    .unwrap();
}
