//! Fields added to a live index (M21.04): no acknowledged write is lost across the
//! change, old slots replay under the extended schema, and the change itself
//! costs the store nothing.
mod support;

use hsearch_engine::{Error as EngineError, Kind, Limits, Query, Value};
use hsearch_index::{Error, Field, Index, Op, Wait};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
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

/// The shared test schema under the scale limits, which admit the writers below.
fn scaled() -> hsearch_engine::Schema {
    schema().with_limits(Limits::scale()).unwrap()
}

/// A document under the evolved schema: `tag`, `n`, then the optional `note`.
fn noted(id: &str, n: i64, note: Option<&str>) -> Op {
    Op::Put {
        id: id.to_string(),
        fields: vec![
            Field::text("t"),
            Field::Int(n),
            note.map_or(Field::Absent, Field::text),
        ],
    }
}

fn count(index: &Index, field: &str, value: &str) -> u64 {
    index
        .snapshot()
        .unwrap()
        .search(
            &Query::Term {
                field,
                value: Value::String(value),
            },
            &[],
            0,
            &[],
        )
        .unwrap()
        .total
}

#[test]
fn a_field_added_while_writers_run_loses_no_write_and_is_searchable_at_once() {
    let (store, scratch) = (store(), Scratch::new("evolve-live"));
    let index =
        Arc::new(Index::create(Arc::clone(&store), "orders", scaled(), &scratch.0).unwrap());
    index
        .bulk(vec![put("old0", "x", 1)], Wait::Searchable)
        .unwrap();

    let writers: Vec<_> = (0..4)
        .map(|w| {
            let index = Arc::clone(&index);
            std::thread::spawn(move || {
                let mut acknowledged = Vec::new();
                for i in 0..60 {
                    let id = format!("w{w}-{i}");
                    // Written under whichever schema is current: two values (the
                    // version before) or three (after); never refused for either.
                    let op = if i % 2 == 0 {
                        put(&id, "t", 10)
                    } else {
                        noted(&id, 10, Some("fresh"))
                    };
                    match index.bulk(vec![op], Wait::Durable) {
                        Ok(_) => acknowledged.push(id),
                        // A three-value document before the field exists is refused
                        // whole: it names a field the schema does not have yet.
                        Err(Error::Invalid) => {}
                        Err(other) => panic!("{other:?}"),
                    }
                }
                acknowledged
            })
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let v2 = scaled().evolve(2, &[("note", Kind::Keyword)]).unwrap();
    index.evolve(v2.clone()).unwrap();
    let acknowledged: Vec<String> = writers
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    index.refresh().unwrap();

    let snapshot = index.snapshot().unwrap();
    let total = snapshot.search(&Query::All, &[], 0, &[]).unwrap().total as usize;
    assert_eq!(
        total,
        acknowledged.len() + 1,
        "every acknowledged write is searchable"
    );
    for id in acknowledged
        .iter()
        .take(40)
        .chain(["old0".to_string()].iter())
    {
        assert!(snapshot.get(id, &["tag"]).unwrap().is_some(), "{id}");
    }
    let fresh = count(&index, "note", "fresh");
    assert!(
        fresh > 0,
        "documents written after the change carry the field"
    );

    // The old document reads back without the field, and with its others intact.
    let old = snapshot
        .get("old0", &["tag", "n", "note"])
        .unwrap()
        .unwrap();
    assert_eq!(old.fields.len(), 2);
}

#[test]
fn old_slots_replay_under_the_extended_schema_after_a_restart() {
    let store = store();
    let (one, two) = (Scratch::new("evolve-a"), Scratch::new("evolve-b"));
    let index = Index::create(Arc::clone(&store), "orders", schema(), &one.0).unwrap();
    index
        .bulk(vec![put("a", "x", 1), put("b", "x", 2)], Wait::Durable)
        .unwrap();
    let v2 = schema().evolve(2, &[("note", Kind::Keyword)]).unwrap();
    index.evolve(v2.clone()).unwrap();
    index
        .bulk(
            vec![noted("c", 3, Some("hello")), noted("d", 4, None)],
            Wait::Durable,
        )
        .unwrap();
    index.refresh().unwrap();
    let before = all(&index.snapshot().unwrap());
    index.close();
    std::fs::remove_dir_all(&one.0).unwrap();

    // A new process that knows only the evolved schema replays all of it.
    let again = Index::open(Arc::clone(&store), "orders", v2, &two.0).unwrap();
    assert_eq!(all(&again.snapshot().unwrap()), before);
    assert_eq!(before.0, 4);
    assert_eq!(count(&again, "note", "hello"), 1);
    // The schema that predates the field cannot replay a slot that carries it.
    drop(again);
    let three = Scratch::new("evolve-c");
    assert!(Index::open(store, "orders", schema(), &three.0).is_err());
}

#[test]
fn a_schema_that_does_not_extend_the_current_one_is_refused() {
    let (store, scratch) = (store(), Scratch::new("evolve-refused"));
    let index = Index::create(store, "orders", schema(), &scratch.0).unwrap();
    let other = hsearch_engine::Schema::new(1, &[("tag", Kind::Text), ("n", Kind::Integer)])
        .unwrap()
        .evolve(2, &[("note", Kind::Keyword)])
        .unwrap();
    assert_eq!(
        index.evolve(other).unwrap_err(),
        Error::Engine(EngineError::Schema)
    );
    // Nothing changed: the index still writes under its schema.
    index
        .bulk(vec![put("a", "x", 1)], Wait::Searchable)
        .unwrap();
}

#[test]
fn an_absent_required_field_is_refused() {
    let (store, scratch) = (store(), Scratch::new("evolve-required"));
    let index = Index::create(store, "orders", schema(), &scratch.0).unwrap();
    let missing = Op::Put {
        id: "a".into(),
        fields: vec![Field::Absent, Field::Int(1)],
    };
    assert!(index.bulk(vec![missing], Wait::Durable).is_err());
}

#[test]
fn a_mapping_is_replaced_only_if_it_is_still_what_the_caller_read() {
    let store = store();
    hsearch_index::put_mapping(&*store, "orders", b"one").unwrap();
    // Not what is stored: refused, and the stored bytes are untouched.
    assert_eq!(
        hsearch_index::replace_mapping(&*store, "orders", b"zero", b"two"),
        Err(Error::Changed)
    );
    assert_eq!(
        hsearch_index::get_mapping(&*store, "orders").unwrap(),
        b"one"
    );
    hsearch_index::replace_mapping(&*store, "orders", b"one", b"two").unwrap();
    assert_eq!(
        hsearch_index::get_mapping(&*store, "orders").unwrap(),
        b"two"
    );
    // A second replace from the stale value loses; the first writer's value stands.
    assert_eq!(
        hsearch_index::replace_mapping(&*store, "orders", b"one", b"three"),
        Err(Error::Changed)
    );
    assert_eq!(
        hsearch_index::get_mapping(&*store, "orders").unwrap(),
        b"two"
    );
    assert_eq!(
        hsearch_index::replace_mapping(&*store, "absent", b"a", b"b"),
        Err(Error::NotFound)
    );
}
