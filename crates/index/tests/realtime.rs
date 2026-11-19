//! The owner's real-time view (M21.05): reads return the latest acknowledged write
//! before any refresh, the memory held for it is bounded, and a refresh that
//! cannot be taken sheds payloads instead of growing.
mod support;

use hsearch_engine::{Limits, StoredValue};
use hsearch_index::{Error, Index, Wait};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use std::sync::Arc;
use support::*;

fn open(label: &str) -> (Index, Scratch) {
    let store = Arc::new(
        MemoryStore::new(MemoryLimits {
            objects: 4096,
            versions: 16384,
            version_bytes: 64 << 20,
            identities: 16384,
            identity_bytes: 64 << 20,
        })
        .unwrap(),
    );
    let scratch = Scratch::new(label);
    let schema = schema().with_limits(Limits::scale()).unwrap();
    (
        Index::create(store, "rt", schema, &scratch.0).unwrap(),
        scratch,
    )
}

fn n_of(index: &Index, id: &str, realtime: bool) -> Option<i64> {
    let hit = index
        .get_many(&[id], &["tag", "n"], realtime)
        .unwrap()
        .pop()?;
    hit.map(|h| match h.fields[1].1 {
        StoredValue::Integer(n) => n,
        ref other => panic!("{other:?}"),
    })
}

#[test]
fn a_durable_write_is_read_before_any_refresh_and_a_projection_is_honoured() {
    let (index, _s) = open("rt-basic");
    index.bulk(vec![put("a", "x", 1)], Wait::Durable).unwrap();
    assert_eq!(n_of(&index, "a", true), Some(1));
    assert_eq!(n_of(&index, "a", false), None, "not searchable yet");
    let hit = index
        .get_many(&["a"], &["n"], true)
        .unwrap()
        .remove(0)
        .unwrap();
    assert_eq!(hit.fields, vec![("n".to_string(), StoredValue::Integer(1))]);
    // A name that is not a field is left out, as the snapshot leaves it out.
    let hit = index
        .get_many(&["a"], &["nope"], true)
        .unwrap()
        .remove(0)
        .unwrap();
    assert!(hit.fields.is_empty());
    index.bulk(vec![put("a", "x", 2)], Wait::Durable).unwrap();
    assert_eq!(n_of(&index, "a", true), Some(2));
    index.bulk(vec![delete("a")], Wait::Durable).unwrap();
    assert_eq!(n_of(&index, "a", true), None);
    index.refresh().unwrap();
    assert_eq!(n_of(&index, "a", false), None);
    // The refresh emptied the record and the snapshot answers the same.
    index
        .bulk(vec![put("b", "y", 5)], Wait::Searchable)
        .unwrap();
    assert_eq!(n_of(&index, "b", false), Some(5));
    assert_eq!(n_of(&index, "b", true), Some(5));
}

fn many(index: &Index, from: usize, to: usize) {
    for chunk in (from..to).step_by(2000) {
        let ops = (chunk..to.min(chunk + 2000))
            .map(|i| put(&i.to_string(), "x", i as i64))
            .collect();
        index.bulk(ops, Wait::Durable).unwrap();
    }
}

#[test]
fn a_record_past_its_bound_is_emptied_by_a_refresh() {
    let (index, _s) = open("rt-bound");
    many(&index, 0, 20_000);
    // Crossing the id bound refreshed once: the oldest are searchable now.
    assert_eq!(n_of(&index, "0", false), Some(0));
    assert_eq!(n_of(&index, "19999", true), Some(19_999));
}

#[test]
fn when_no_refresh_can_be_taken_payloads_are_shed_and_a_read_refreshes_first() {
    let (index, _s) = open("rt-shed");
    index
        .bulk(vec![put("seed", "x", 0)], Wait::Searchable)
        .unwrap();
    // The engine lends at most four snapshots; hold them so a refresh is refused.
    let held: Vec<_> = (0..1).map(|_| index.snapshot().unwrap()).collect();
    // Take the remaining slots by refreshing and holding each new one.
    let mut more = Vec::new();
    for _ in 0..3 {
        index.refresh().unwrap();
        more.push(index.snapshot().unwrap());
    }
    many(&index, 1, 22_000);
    // Payloads were shed, not grown; the newest write is read once a slot is free.
    drop((held, more));
    assert_eq!(n_of(&index, "21999", true), Some(21_999));
    assert_eq!(n_of(&index, "1", true), Some(1));
    assert_eq!(
        n_of(&index, "21999", false),
        Some(21_999),
        "the shed id's read refreshed"
    );
}

#[test]
fn work_is_refused_not_buffered_when_no_refresh_can_be_taken_for_too_long() {
    let (index, _s) = open("rt-hard");
    index
        .bulk(vec![put("seed", "x", 0)], Wait::Searchable)
        .unwrap();
    let mut held = vec![index.snapshot().unwrap()];
    for _ in 0..3 {
        index.refresh().unwrap();
        held.push(index.snapshot().unwrap());
    }
    let mut refused = None;
    for chunk in 0..40 {
        let ops = (0..2000)
            .map(|i| put(&(chunk * 2000 + i + 1).to_string(), "x", 1))
            .collect();
        if let Err(error) = index.bulk(ops, Wait::Durable) {
            refused = Some(error);
            break;
        }
    }
    assert_eq!(refused, Some(Error::Busy));
    // Once a slot is free the writer takes the refresh and accepts work again.
    drop(held);
    index
        .bulk(vec![put("after", "x", 9)], Wait::Durable)
        .unwrap();
    assert_eq!(n_of(&index, "after", true), Some(9));
}
