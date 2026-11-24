//! A served index found by name: created once, recovered after a restart with every acknowledged
//! write, fenced when another opens it, and separate from every other name. In-memory store, so
//! the numbers are provisional.
mod support;

use std::sync::Arc;

use hsearch_engine::{Kind, Schema, StoredValue};
use hsearch_index::{Error, Field, Op, Wait};
use hsearch_lane::{block_on, create_lane};
use hsearch_store::served::{ServedIndex, Settings, index_id};
use support::*;

fn schema() -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword)]).unwrap()
}

fn settings() -> Settings {
    let mut s = Settings::new(layout());
    s.window = std::time::Duration::ZERO;
    s.base_every_batches = 0;
    s
}

fn scratch(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hsearch-served-{label}-{}", std::process::id()))
}

fn put(id: &str, tag: &str) -> Op {
    Op::Put {
        id: id.into(),
        fields: vec![Field::Str(Arc::new(tag.into()))],
    }
}

fn create(store: &Arc<Crash>, name: &str, label: &str) -> Result<ServedIndex, Error> {
    ServedIndex::create(
        store,
        &settings(),
        name,
        schema(),
        &[3; 32],
        &scratch(label),
    )
}

fn open(store: &Arc<Crash>, name: &str, label: &str) -> Result<ServedIndex, Error> {
    ServedIndex::open(
        store,
        &settings(),
        name,
        schema(),
        &[3; 32],
        &scratch(label),
    )
}

fn tag(index: &ServedIndex, id: &str, realtime: bool) -> Option<String> {
    index.get_many(&[id], &["tag"], realtime).unwrap()[0]
        .as_ref()
        .map(|hit| {
            let StoredValue::String(tag) = &hit.fields[0].1 else {
                panic!("tag is a string")
            };
            tag.clone()
        })
}

#[test]
fn an_index_is_created_once_and_a_missing_one_is_not_found() {
    let store = Crash::new();
    assert!(matches!(
        open(&store, "books", "missing"),
        Err(Error::NotFound)
    ));
    let books = create(&store, "books", "once").unwrap();
    // Nothing is in it, and it can still be searched.
    assert!(books.snapshot().is_ok());
    assert!(matches!(
        create(&store, "books", "twice"),
        Err(Error::Exists)
    ));
    drop(books);
    assert!(open(&store, "books", "reopened").is_ok());
}

#[test]
fn acknowledged_writes_survive_a_restart() {
    let store = Crash::new();
    let books = create(&store, "books", "restart").unwrap();
    books.bulk(vec![put("a", "1")], Wait::Searchable).unwrap();
    books.bulk(vec![put("b", "2")], Wait::Durable).unwrap();
    // Not yet searchable, but a real-time read sees it.
    assert_eq!(tag(&books, "b", false), None);
    assert_eq!(tag(&books, "b", true).as_deref(), Some("2"));
    assert_eq!(tag(&books, "missing", true), None);
    // The real-time read published nothing.
    assert_eq!(tag(&books, "b", false), None);
    drop(books);
    let books = open(&store, "books", "restart").unwrap();
    assert_eq!(tag(&books, "a", false).as_deref(), Some("1"));
    assert_eq!(tag(&books, "b", false).as_deref(), Some("2"));
    assert!(!books.is_broken());
}

#[test]
fn an_empty_index_answers_reads_with_nothing() {
    let store = Crash::new();
    let books = create(&store, "books", "empty").unwrap();
    assert_eq!(tag(&books, "a", true), None);
    assert!(books.bulk(vec![], Wait::Durable).is_err());
}

#[test]
fn another_open_fences_the_first_writer() {
    let store = Crash::new();
    let first = create(&store, "books", "first").unwrap();
    first.bulk(vec![put("a", "1")], Wait::Durable).unwrap();
    let second = open(&store, "books", "second").unwrap();
    let err = first.bulk(vec![put("a", "2")], Wait::Durable).unwrap_err();
    assert!(
        matches!(
            err,
            Error::AppliedFailed | Error::Unknown | Error::Fenced | Error::Poisoned
        ),
        "{err:?}"
    );
    second.bulk(vec![put("a", "3")], Wait::Searchable).unwrap();
    assert_eq!(tag(&second, "a", false).as_deref(), Some("3"));
}

#[test]
fn names_do_not_share_a_log() {
    let store = Crash::new();
    let a = create(&store, "alpha", "alpha").unwrap();
    let b = create(&store, "beta", "beta").unwrap();
    a.bulk(vec![put("x", "from-a")], Wait::Searchable).unwrap();
    assert_eq!(tag(&b, "x", true), None);
    b.bulk(vec![put("x", "from-b")], Wait::Searchable).unwrap();
    assert_eq!(tag(&a, "x", false).as_deref(), Some("from-a"));
    assert_ne!(index_id("alpha"), index_id("beta"));
}

#[test]
fn a_creation_that_stopped_after_the_lane_is_finished() {
    let store = Crash::new();
    // The lane exists; the head does not.
    let s = settings();
    let (domain, group) = hsearch_lane::dedicated_group(&s.policy, &index_id("books"), b"served");
    block_on(create_lane(
        store.inner_ref(),
        &s.layout,
        &hsearch_lane::LaneId::new(domain),
        &s.policy,
        group,
        1,
        &[index_id("books")],
    ))
    .unwrap();
    assert!(matches!(
        open(&store, "books", "half"),
        Err(Error::NotFound)
    ));
    let books = create(&store, "books", "finish").unwrap();
    books.bulk(vec![put("a", "1")], Wait::Searchable).unwrap();
    assert!(matches!(
        create(&store, "books", "again"),
        Err(Error::Exists)
    ));
}

#[test]
fn a_document_the_engine_refuses_fails_alone() {
    let store = Crash::new();
    let books = create(&store, "books", "refused").unwrap();
    let huge = "h".repeat(200_000);
    let items = books
        .bulk(
            vec![put("a", "1"), put("b", &huge), put("c", "3")],
            Wait::Searchable,
        )
        .unwrap();
    assert!(items[0].is_ok() && items[2].is_ok(), "{items:?}");
    assert!(matches!(items[1], Err(Error::Engine(_))), "{items:?}");
    assert_eq!(tag(&books, "a", false).as_deref(), Some("1"));
    assert_eq!(tag(&books, "b", false), None);
    assert_eq!(tag(&books, "c", false).as_deref(), Some("3"));
}

#[test]
fn a_batch_over_the_record_bound_between_small_ones_survives_a_restart_unpublished() {
    let store = Crash::new();
    let big_schema = || {
        Schema::new(1, &[("tag", Kind::Text)])
            .unwrap()
            .with_limits(hsearch_engine::Limits::scale().with_document_bytes(4 << 20))
            .unwrap()
    };
    let dir = scratch("big");
    let books =
        ServedIndex::create(&store, &settings(), "big", big_schema(), &[3; 32], &dir).unwrap();
    let big = "word ".repeat((hsearch_format::MAX_INLINE_RECORD_BYTES + 1024) / 5);
    let results = books
        .bulk(
            vec![put("a", "1"), put("big", &big), put("c", "3")],
            Wait::Durable,
        )
        .unwrap();
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    // Durable, never refreshed or closed: dropped as a kill would leave it.
    drop(books);
    let books =
        ServedIndex::open(&store, &settings(), "big", big_schema(), &[3; 32], &dir).unwrap();
    assert_eq!(tag(&books, "a", true).as_deref(), Some("1"));
    assert_eq!(tag(&books, "big", true).as_deref(), Some(big.as_str()));
    assert_eq!(tag(&books, "c", true).as_deref(), Some("3"));
}
