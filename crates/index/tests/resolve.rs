//! M04.03: resolved-operation preparation. Pure functions of committed state and
//! the request: no clock, no randomness, no I/O.
use hsearch_engine::Kind;
use hsearch_index::resolve::{
    Committed, CommittedState, Cond, Item, Lookup, Outcome, OutcomeStore, Reject, Request, Resolver,
};
use hsearch_index::{Field, Op};
use std::collections::HashMap;

#[derive(Default, Clone)]
struct State {
    head: u64,
    docs: HashMap<String, Committed>,
}
impl CommittedState for State {
    fn head(&self) -> u64 {
        self.head
    }
    fn get(&self, id: &str) -> Option<Committed> {
        self.docs.get(id).cloned()
    }
}
impl State {
    /// What a durable slot does: the only way committed state advances.
    fn commit(&mut self, first_version: i64, term: u64, ops: &[Op]) {
        for (i, op) in ops.iter().enumerate() {
            let seq_no = (first_version as u64) + i as u64;
            match op {
                Op::Put { id, fields } => {
                    self.docs.insert(
                        id.clone(),
                        Committed {
                            seq_no,
                            primary_term: term,
                            fields: fields.clone(),
                        },
                    );
                }
                Op::Delete { id } => {
                    self.docs.remove(id);
                }
            }
        }
        self.head += 1;
    }
}

const FIELDS: [(&str, Kind); 3] = [
    ("title", Kind::Text),
    ("price", Kind::Integer),
    ("active", Kind::Boolean),
];

fn resolver() -> Resolver {
    Resolver::new(1, &FIELDS, 1 << 20)
}
fn doc(title: &str, price: i64, active: bool) -> Vec<Field> {
    vec![Field::text(title), Field::Int(price), Field::Bool(active)]
}
fn index(id: &str, d: Vec<Field>) -> Request {
    Request::Index {
        id: id.into(),
        fields: d,
        cond: Cond::none(),
    }
}
fn update(id: &str, part: &[(&str, Field)]) -> Request {
    Request::Update {
        id: id.into(),
        doc: part
            .iter()
            .map(|(n, f)| (n.to_string(), f.clone()))
            .collect(),
        upsert: None,
        doc_as_upsert: false,
        detect_noop: true,
        cond: Cond::none(),
    }
}
fn seeded() -> State {
    let r = resolver();
    let mut s = State::default();
    let p = r.prepare(&s, 1, &[index("a", doc("x", 1, true))]).unwrap();
    s.commit(1, 1, p.ops());
    s
}
fn outcomes(items: &[Item]) -> Vec<Outcome> {
    items.iter().map(|i| i.outcome).collect()
}

#[test]
fn partial_update_merges_over_committed_source_and_logs_a_whole_put() {
    let s = seeded();
    let p = resolver()
        .prepare(&s, 2, &[update("a", &[("price", Field::Int(9))])])
        .unwrap();
    assert_eq!(
        p.ops(),
        &[Op::Put {
            id: "a".into(),
            fields: doc("x", 9, true)
        }]
    );
    assert_eq!(outcomes(p.items()), [Outcome::Updated]);
    assert_eq!(p.items()[0].seq_no, 2);
}

#[test]
fn detect_noop_logs_nothing_and_consumes_no_version() {
    let s = seeded();
    let p = resolver()
        .prepare(
            &s,
            2,
            &[
                update("a", &[("price", Field::Int(1))]),
                index("b", doc("y", 2, false)),
            ],
        )
        .unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::NoOp, Outcome::Created]);
    assert_eq!(p.ops().len(), 1);
    assert_eq!(
        p.items()[0].seq_no,
        1,
        "a no-op reports the committed seq_no"
    );
    assert_eq!(
        p.items()[1].seq_no,
        2,
        "the next applied op takes the first version"
    );
}

#[test]
fn detect_noop_off_still_logs_an_identical_update() {
    let s = seeded();
    let mut r = update("a", &[("price", Field::Int(1))]);
    if let Request::Update { detect_noop, .. } = &mut r {
        *detect_noop = false;
    }
    let p = resolver().prepare(&s, 2, &[r]).unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Updated]);
}

#[test]
fn update_of_a_missing_document_needs_an_upsert() {
    let s = seeded();
    let p = resolver()
        .prepare(&s, 2, &[update("zz", &[("price", Field::Int(1))])])
        .unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Missing)]);
    assert!(p.ops().is_empty());
    let up = Request::Update {
        id: "zz".into(),
        doc: vec![("price".into(), Field::Int(5))],
        upsert: Some(doc("n", 0, false)),
        doc_as_upsert: false,
        detect_noop: true,
        cond: Cond::none(),
    };
    let p = resolver().prepare(&s, 2, &[up]).unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Created]);
    assert_eq!(
        p.ops(),
        &[Op::Put {
            id: "zz".into(),
            fields: doc("n", 0, false)
        }]
    );
}

#[test]
fn doc_as_upsert_needs_every_field_and_a_partial_one_is_invalid() {
    let s = seeded();
    let mk_id = |id: &str, d: Vec<(&str, Field)>| Request::Update {
        id: id.into(),
        doc: d.into_iter().map(|(n, f)| (n.to_string(), f)).collect(),
        upsert: None,
        doc_as_upsert: true,
        detect_noop: true,
        cond: Cond::none(),
    };
    let mk = |d| mk_id("zz", d);
    let full = mk(vec![
        ("title", Field::text("t")),
        ("price", Field::Int(1)),
        ("active", Field::Bool(true)),
    ]);
    let p = resolver()
        .prepare(&s, 2, &[full, mk_id("yy", vec![("price", Field::Int(1))])])
        .unwrap();
    assert_eq!(
        outcomes(p.items()),
        [Outcome::Created, Outcome::Rejected(Reject::Invalid)]
    );
}

#[test]
fn sequence_and_term_conditions_fence_stale_writers() {
    let s = seeded(); // a is at seq_no 1, term 1
    let r = resolver();
    let cond = |n, t| Cond::new(Some(n), Some(t));
    let idx = |c| Request::Index {
        id: "a".into(),
        fields: doc("z", 3, true),
        cond: c,
    };
    let p = r.prepare(&s, 2, &[idx(cond(1, 1))]).unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Updated]);
    for stale in [cond(0, 1), cond(1, 2)] {
        let p = r.prepare(&s, 2, &[idx(stale)]).unwrap();
        assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Conflict)]);
        assert!(p.ops().is_empty());
    }
    // One half of the pair is a malformed request, not a conflict.
    let p = r.prepare(&s, 2, &[idx(Cond::new(Some(1), None))]).unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Invalid)]);
    // A condition on a missing document conflicts; on delete too.
    let del = Request::Delete {
        id: "nope".into(),
        cond: cond(1, 1),
    };
    let p = r.prepare(&s, 2, &[del]).unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Conflict)]);
}

#[test]
fn a_recreated_document_cannot_be_overwritten_by_a_token_from_its_past_life() {
    let r = resolver();
    let mut s = seeded(); // a: seq 1
    let p = r
        .prepare(
            &s,
            2,
            &[Request::Delete {
                id: "a".into(),
                cond: Cond::none(),
            }],
        )
        .unwrap();
    s.commit(2, 1, p.ops());
    let p = r
        .prepare(&s, 3, &[index("a", doc("again", 1, true))])
        .unwrap();
    s.commit(3, 1, p.ops());
    let old = Request::Index {
        id: "a".into(),
        fields: doc("stale", 0, false),
        cond: Cond::new(Some(1), Some(1)),
    };
    let p = r.prepare(&s, 4, &[old]).unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Conflict)]);
}

#[test]
fn create_races_resolve_in_order_against_committed_state_plus_earlier_items() {
    let s = State::default();
    let r = resolver();
    let create = |t: &str| Request::Create {
        id: "k".into(),
        fields: doc(t, 1, true),
    };
    let p = r
        .prepare(&s, 1, &[create("first"), create("second")])
        .unwrap();
    assert_eq!(
        outcomes(p.items()),
        [Outcome::Created, Outcome::Rejected(Reject::Exists)]
    );
    // Dependent updates in one batch see each other: the batch is one slot.
    let p = r
        .prepare(
            &s,
            1,
            &[
                index("d", doc("v0", 0, false)),
                update("d", &[("price", Field::Int(1))]),
                update("d", &[("price", Field::Int(2))]),
            ],
        )
        .unwrap();
    let last = p.ops().last().unwrap();
    assert_eq!(
        *last,
        Op::Put {
            id: "d".into(),
            fields: doc("v0", 2, false)
        }
    );
    assert_eq!(
        p.items().iter().map(|i| i.seq_no).collect::<Vec<_>>(),
        [1, 2, 3]
    );
}

#[test]
fn a_dropped_preparation_is_not_state_so_the_next_one_reads_committed_state_only() {
    let s = State::default();
    let r = resolver();
    let first = r
        .prepare(&s, 1, &[index("k", doc("speculative", 1, true))])
        .unwrap();
    drop(first); // the slot failed to commit: nothing was applied to `s`
    let again = r
        .prepare(&s, 1, &[update("k", &[("price", Field::Int(5))])])
        .unwrap();
    assert_eq!(
        outcomes(again.items()),
        [Outcome::Rejected(Reject::Missing)],
        "an update must not see a document that never committed"
    );
}

#[test]
fn a_preparation_is_stale_once_any_slot_committed_after_it() {
    let mut s = State::default();
    let r = resolver();
    let a = r.prepare(&s, 1, &[index("k", doc("a", 1, true))]).unwrap();
    let b = r.prepare(&s, 1, &[index("k", doc("b", 1, true))]).unwrap();
    assert!(a.ensure_current(s.head()).is_ok());
    s.commit(1, 1, a.ops());
    assert!(
        b.ensure_current(s.head()).is_err(),
        "b was resolved against a head that moved"
    );
}

#[test]
fn preparation_is_repeatable_and_logged_ops_alone_reproduce_committed_state() {
    let r = resolver();
    let reqs = [
        index("a", doc("x", 1, true)),
        update("a", &[("price", Field::Int(7))]),
        Request::Delete {
            id: "a".into(),
            cond: Cond::none(),
        },
        index("b", doc("y", 2, false)),
    ];
    let s0 = State::default();
    let p1 = r.prepare(&s0, 1, &reqs).unwrap();
    let p2 = r.prepare(&s0, 1, &reqs).unwrap();
    assert_eq!(p1.ops(), p2.ops());
    assert_eq!(p1.items(), p2.items());
    // Replay applies the logged ops only; it never calls the resolver, so a
    // different clock or random source cannot change the result.
    let mut live = State::default();
    live.commit(1, 1, p1.ops());
    let mut replayed = State::default();
    replayed.commit(1, 1, p1.ops());
    assert_eq!(live.docs.len(), 1);
    assert_eq!(
        live.docs.get("b").map(|d| d.fields.clone()),
        Some(doc("y", 2, false))
    );
    assert_eq!(
        live.docs.keys().collect::<Vec<_>>(),
        replayed.docs.keys().collect::<Vec<_>>()
    );
}

#[test]
fn unknown_duplicate_and_mistyped_update_fields_are_invalid_not_ignored() {
    let s = seeded();
    let r = resolver();
    let cases = [
        update("a", &[("nope", Field::Int(1))]),
        update("a", &[("price", Field::Int(1)), ("price", Field::Int(2))]),
        update("a", &[("price", Field::text("1"))]),
        update("a", &[]),
    ];
    for c in cases {
        let p = r.prepare(&s, 2, &[c]).unwrap();
        assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Invalid)]);
    }
}

#[test]
fn a_merged_document_over_the_byte_bound_is_a_limit_refusal() {
    let s = seeded();
    let r = Resolver::new(1, &FIELDS, 16);
    let p = r
        .prepare(
            &s,
            2,
            &[update("a", &[("title", Field::text("x".repeat(64)))])],
        )
        .unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Limit)]);
}

#[test]
fn a_batch_over_the_slot_op_bound_is_refused_whole() {
    let s = State::default();
    let reqs: Vec<_> = (0..4097)
        .map(|i| index(&format!("k{i}"), doc("x", 1, true)))
        .collect();
    assert!(resolver().prepare(&s, 1, &reqs).is_err());
}

#[test]
fn the_outcome_store_replays_a_recorded_request_and_is_bounded() {
    let s = State::default();
    let r = resolver();
    let p = r.prepare(&s, 1, &[index("k", doc("x", 1, true))]).unwrap();
    let mut store = OutcomeStore::new(2, 1 << 20);
    assert_eq!(store.get("req-1", 7), Lookup::Unknown);
    store.record("req-1", 7, p.items().to_vec());
    assert_eq!(store.get("req-1", 7), Lookup::Replay(p.items()));
    assert_eq!(
        store.get("req-1", 8),
        Lookup::Mismatch,
        "same id, different body"
    );
    store.record("req-2", 1, vec![]);
    store.record("req-3", 1, vec![]);
    assert_eq!(
        store.get("req-1", 7),
        Lookup::Unknown,
        "oldest entry evicted at the bound"
    );
    assert!(matches!(store.get("req-3", 1), Lookup::Replay(_)));
    assert!(store.len() <= 2);
}

#[test]
fn the_outcome_store_is_bounded_in_bytes_too() {
    let item = |n: usize| Item {
        id: "i".repeat(n),
        seq_no: 1,
        primary_term: 1,
        outcome: Outcome::Created,
    };
    let mut store = OutcomeStore::new(100, 700);
    store.record("a", 1, vec![item(200)]);
    store.record("b", 1, vec![item(200)]);
    store.record("c", 1, vec![item(200)]); // 891 estimated bytes against 700: the oldest goes
    assert_eq!(store.get("a", 1), Lookup::Unknown);
    assert!(matches!(store.get("c", 1), Lookup::Replay(_)));
    store.record("huge", 1, vec![item(5000)]);
    assert_eq!(
        store.get("huge", 1),
        Lookup::Unknown,
        "an entry over the bound is never kept"
    );
}

#[test]
fn ids_the_slot_cannot_decode_are_refused_before_logging() {
    let s = State::default();
    let r = resolver();
    let p = r
        .prepare(
            &s,
            1,
            &[
                index(&"a".repeat(257), doc("x", 1, true)),
                index("", doc("x", 1, true)),
                index(&"a".repeat(256), doc("x", 1, true)),
            ],
        )
        .unwrap();
    assert_eq!(
        outcomes(p.items()),
        [
            Outcome::Rejected(Reject::Invalid),
            Outcome::Rejected(Reject::Invalid),
            Outcome::Created
        ]
    );
}

#[test]
fn whole_documents_must_match_the_schema_kinds() {
    let s = State::default();
    let r = resolver();
    let bad = vec![Field::text("x"), Field::text("1"), Field::Bool(true)];
    let p = r.prepare(&s, 1, &[index("k", bad)]).unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Invalid)]);
}

#[test]
fn a_version_counter_near_the_end_of_the_range_is_refused_not_wrapped() {
    let s = State::default();
    let r = resolver();
    assert!(
        r.prepare(&s, i64::MAX - 10, &[index("k", doc("x", 1, true))])
            .is_err()
    );
}

#[test]
fn conditions_apply_to_updates() {
    let s = seeded(); // a: seq 1, term 1
    let r = resolver();
    let upd = |id: &str, price, cond| Request::Update {
        id: id.into(),
        doc: vec![("price".into(), Field::Int(price))],
        upsert: None,
        doc_as_upsert: false,
        detect_noop: true,
        cond,
    };
    let ok = r
        .prepare(&s, 2, &[upd("a", 9, Cond::new(Some(1), Some(1)))])
        .unwrap();
    assert_eq!(outcomes(ok.items()), [Outcome::Updated]);
    for stale in [Cond::new(Some(2), Some(1)), Cond::new(Some(1), Some(2))] {
        let p = r.prepare(&s, 2, &[upd("a", 9, stale)]).unwrap();
        assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Conflict)]);
        assert!(p.ops().is_empty());
    }
    let p = r
        .prepare(&s, 2, &[upd("a", 9, Cond::new(Some(1), None))])
        .unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Invalid)]);
    // A condition on a document that is not there conflicts (documented difference).
    let p = r
        .prepare(&s, 2, &[upd("zz", 9, Cond::new(Some(1), Some(1)))])
        .unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Conflict)]);
    // A condition cannot ride with an upsert.
    let with_upsert = Request::Update {
        id: "a".into(),
        doc: vec![("price".into(), Field::Int(9))],
        upsert: Some(doc("n", 0, false)),
        doc_as_upsert: false,
        detect_noop: true,
        cond: Cond::new(Some(1), Some(1)),
    };
    let p = r.prepare(&s, 2, &[with_upsert]).unwrap();
    assert_eq!(outcomes(p.items()), [Outcome::Rejected(Reject::Invalid)]);
}

#[test]
fn a_batch_cannot_log_more_text_than_one_slot_may_carry() {
    let s = State::default();
    // Per-document bound 6 MiB; the slot cap is the larger of 8 MiB and that.
    let r = Resolver::new(1, &FIELDS, 6 << 20);
    let big = |id: &str| index(id, doc(&"x".repeat(5 << 20), 1, true));
    let p = r.prepare(&s, 1, &[big("a"), big("b")]).unwrap();
    assert_eq!(
        outcomes(p.items()),
        [Outcome::Created, Outcome::Rejected(Reject::Limit)]
    );
    assert_eq!(p.ops().len(), 1);
}

#[test]
fn a_slot_committing_during_preparation_makes_it_stale() {
    use std::cell::Cell;
    // `get` models a slot committing between two reads.
    struct Racy(Cell<u64>);
    impl CommittedState for Racy {
        fn head(&self) -> u64 {
            self.0.get()
        }
        fn get(&self, _id: &str) -> Option<Committed> {
            self.0.set(self.0.get() + 1);
            None
        }
    }
    let s = Racy(Cell::new(10));
    let p = resolver()
        .prepare(&s, 1, &[index("k", doc("x", 1, true))])
        .unwrap();
    assert!(
        p.ensure_current(s.head()).is_err(),
        "resolved at head 10, committed head is 11"
    );
}
