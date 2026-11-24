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

mod resolved {
    use super::*;
    use hsearch_index::resolve::{Cond, Outcome, Reject, Request};
    use hsearch_store::host::RetryKey;

    fn doc(tag: &str) -> Vec<Field> {
        vec![Field::Str(Arc::new(tag.into()))]
    }

    fn index(id: &str, tag: &str, cond: Cond) -> Request {
        Request::Index {
            id: id.into(),
            fields: doc(tag),
            cond,
        }
    }

    fn update(id: &str, tag: &str, upsert: bool, noop: bool, cond: Cond) -> Request {
        Request::Update {
            id: id.into(),
            doc: vec![("tag".into(), Field::Str(Arc::new(tag.into())))],
            upsert: upsert.then(|| doc(tag)),
            doc_as_upsert: false,
            detect_noop: noop,
            cond,
        }
    }

    fn one(index: &ServedIndex, request: Request) -> hsearch_index::resolve::Item {
        index
            .resolve(vec![request], None, Wait::Durable)
            .unwrap()
            .remove(0)
    }

    #[test]
    fn update_upsert_condition_and_noop_resolve_against_committed_state() {
        let store = Crash::new();
        let books = create(&store, "res", "res").unwrap();
        // Missing without an upsert; an upsert creates.
        let missing = one(&books, update("a", "x", false, false, Cond::none()));
        assert_eq!(missing.outcome, Outcome::Rejected(Reject::Missing));
        let created = one(&books, update("a", "x", true, false, Cond::none()));
        assert_eq!(created.outcome, Outcome::Created);
        assert_eq!(tag(&books, "a", true).as_deref(), Some("x"));
        // A partial update replaces and takes a higher version.
        let updated = one(&books, update("a", "y", false, false, Cond::none()));
        assert_eq!(updated.outcome, Outcome::Updated);
        assert!(updated.seq_no > created.seq_no);
        assert_eq!(tag(&books, "a", true).as_deref(), Some("y"));
        // The same value with detect_noop writes nothing and reports the committed version.
        let noop = one(&books, update("a", "y", false, true, Cond::none()));
        assert_eq!(noop.outcome, Outcome::NoOp);
        assert_eq!(noop.seq_no, updated.seq_no);
        // A condition on the stale version conflicts; on the current one it passes.
        let stale = Cond::new(Some(created.seq_no), Some(created.primary_term));
        let conflict = one(&books, index("a", "z", stale));
        assert_eq!(conflict.outcome, Outcome::Rejected(Reject::Conflict));
        assert_eq!(tag(&books, "a", true).as_deref(), Some("y"));
        let now = Cond::new(Some(updated.seq_no), Some(updated.primary_term));
        assert_eq!(one(&books, index("a", "z", now)).outcome, Outcome::Updated);
        // Create refuses an existing document; delete of a missing one is NotFound.
        let exists = one(
            &books,
            Request::Create {
                id: "a".into(),
                fields: doc("w"),
            },
        );
        assert_eq!(exists.outcome, Outcome::Rejected(Reject::Exists));
        let gone = one(
            &books,
            Request::Delete {
                id: "nope".into(),
                cond: Cond::none(),
            },
        );
        assert_eq!(gone.outcome, Outcome::NotFound);
    }

    #[test]
    fn resolved_writes_recover_from_the_log_without_the_resolver() {
        let store = Crash::new();
        let books = create(&store, "res", "res-recover").unwrap();
        let first = one(&books, update("a", "x", true, false, Cond::none()));
        let second = one(&books, update("a", "y", false, false, Cond::none()));
        one(&books, update("b", "k", true, false, Cond::none()));
        one(
            &books,
            Request::Delete {
                id: "b".into(),
                cond: Cond::none(),
            },
        );
        drop(books);
        let books = open(&store, "res", "res-recover").unwrap();
        assert_eq!(tag(&books, "a", false).as_deref(), Some("y"));
        assert_eq!(tag(&books, "b", false), None);
        // The recovered version is the one acknowledged, so its condition still matches.
        let cond = Cond::new(Some(second.seq_no), Some(second.primary_term));
        assert!(second.seq_no > first.seq_no);
        assert_eq!(one(&books, index("a", "z", cond)).outcome, Outcome::Updated);
    }

    #[test]
    fn a_mixed_batch_pairs_each_item_with_what_was_written_and_an_id_is_named_once() {
        let store = Crash::new();
        let books = create(&store, "res", "res-mixed").unwrap();
        let first = one(&books, index("keep", "k", Cond::none()));
        let items = books
            .resolve(
                vec![
                    index("a", "1", Cond::none()),
                    update("missing", "x", false, false, Cond::none()),
                    update("keep", "k", false, true, Cond::none()),
                    Request::Delete {
                        id: "keep2".into(),
                        cond: Cond::none(),
                    },
                    index("b", "2", Cond::none()),
                ],
                None,
                Wait::Durable,
            )
            .unwrap();
        let outcomes: Vec<_> = items.iter().map(|i| i.outcome).collect();
        assert_eq!(
            outcomes,
            [
                Outcome::Created,
                Outcome::Rejected(Reject::Missing),
                Outcome::NoOp,
                Outcome::NotFound,
                Outcome::Created
            ]
        );
        // Written items carry the versions the log assigned (what a read reports); the no-op the
        // committed one.
        let version = |id: &str| {
            books.get_many(&[id], &["tag"], true).unwrap()[0]
                .as_ref()
                .map(|h| h.version)
                .unwrap()
        };
        assert_eq!(items[0].seq_no as i64, version("a"));
        assert_eq!(items[4].seq_no as i64, version("b"));
        assert_eq!(items[2].seq_no, first.seq_no);
        assert!(items[0].seq_no < items[4].seq_no);
        // An id named twice in one batch is refused before anything is written.
        let twice = books.resolve(
            vec![index("c", "1", Cond::none()), index("c", "2", Cond::none())],
            None,
            Wait::Durable,
        );
        assert_eq!(twice.unwrap_err(), Error::Invalid);
        assert_eq!(tag(&books, "c", true), None);
    }

    #[test]
    fn a_retry_honours_its_own_wait() {
        let store = Crash::new();
        let books = create(&store, "res", "res-wait").unwrap();
        let key = || {
            Some(RetryKey {
                request: "r".into(),
                fingerprint: 1,
            })
        };
        books
            .resolve(vec![index("a", "x", Cond::none())], key(), Wait::Durable)
            .unwrap();
        assert_eq!(tag(&books, "a", false), None);
        books
            .resolve(vec![index("a", "x", Cond::none())], key(), Wait::Searchable)
            .unwrap();
        assert_eq!(tag(&books, "a", false).as_deref(), Some("x"));
    }

    #[test]
    fn a_retry_gets_the_first_answer_and_a_reused_id_with_another_body_is_refused() {
        let store = Crash::new();
        let books = create(&store, "res", "res-retry").unwrap();
        let key = |fingerprint| {
            Some(RetryKey {
                request: "req-1".into(),
                fingerprint,
            })
        };
        let first = books
            .resolve(
                vec![update("a", "x", true, false, Cond::none())],
                key(7),
                Wait::Durable,
            )
            .unwrap();
        // Another write moves the document on; the retry must not see it.
        one(&books, index("a", "other", Cond::none()));
        let again = books
            .resolve(
                vec![update("a", "x", true, false, Cond::none())],
                key(7),
                Wait::Durable,
            )
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(tag(&books, "a", true).as_deref(), Some("other"));
        let reused = books.resolve(
            vec![update("a", "x", true, false, Cond::none())],
            key(8),
            Wait::Durable,
        );
        assert!(reused.is_err());
    }
}

#[test]
fn a_partial_update_is_refused_where_the_committed_view_carries_no_vector() {
    use hsearch_index::resolve::{Cond, Request};
    let store = Crash::new();
    let spec = hsearch_engine::VectorSpec::hnsw(2, hsearch_engine::Similarity::L2);
    let schema = || Schema::new(1, &[("tag", Kind::Keyword), ("v", Kind::Vector(spec))]).unwrap();
    let books = ServedIndex::create(
        &store,
        &settings(),
        "vec",
        schema(),
        &[3; 32],
        &scratch("vec"),
    )
    .unwrap();
    books
        .bulk(
            vec![Op::Put {
                id: "a".into(),
                fields: vec![
                    Field::Str(Arc::new("w".into())),
                    Field::Vector(Arc::from(vec![1.0f32, 2.0])),
                ],
            }],
            Wait::Durable,
        )
        .unwrap();
    let request = Request::Update {
        id: "a".into(),
        doc: vec![("tag".into(), Field::Str(Arc::new("x".into())))],
        upsert: None,
        doc_as_upsert: false,
        detect_noop: false,
        cond: Cond::none(),
    };
    assert_eq!(
        books
            .resolve(vec![request], None, Wait::Durable)
            .unwrap_err(),
        Error::Invalid
    );
}
