//! A serving engine rebuilt from the committed log alone matches an independent ledger of the
//! documents, versions and request outcomes the batches imply.
mod support;

use std::collections::BTreeMap;

use hsearch_commit::{Committer, Receipt, Request};
use hsearch_engine::{
    Error as EngineError, Kind, Mutation, Outcome, Schema, StoredValue, Value, encode_batch,
};
use hsearch_lane::{block_on, read_head};
use hsearch_store::publish::AnyDependencies;
use hsearch_store::rebuild::{RebuildError, rebuild};
use hsearch_store::recover::{Expect, NotReady};
use support::*;

fn schema() -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword)]).unwrap()
}

/// One batch of the history, written by a model that never touches the engine.
struct Step {
    id: &'static str,
    version: i64,
    /// `None` deletes.
    tag: Option<&'static str>,
}

fn history() -> Vec<Vec<Step>> {
    let put = |id, version, tag| Step {
        id,
        version,
        tag: Some(tag),
    };
    vec![
        vec![put("a", 1, "x"), put("b", 1, "y")],
        vec![put("a", 2, "z")],
        vec![Step {
            id: "b",
            version: 3,
            tag: None,
        }],
        vec![put("c", 4, "w"), put("d", 4, "w")],
        vec![Step {
            id: "never",
            version: 5,
            tag: None,
        }],
    ]
}

fn body(i: usize, steps: &[Step]) -> Vec<u8> {
    let fields: Vec<[(&str, Value<'_>); 1]> = steps
        .iter()
        .map(|s| [("tag", Value::String(s.tag.unwrap_or("")))])
        .collect();
    let mutations: Vec<Mutation<'_>> = steps
        .iter()
        .zip(&fields)
        .map(|(s, f)| match s.tag {
            Some(_) => Mutation::Put {
                id: s.id,
                version: s.version,
                fields: f,
            },
            None => Mutation::Delete {
                id: s.id,
                version: s.version,
            },
        })
        .collect();
    encode_batch(&format!("r{i}"), &mutations).unwrap()
}

fn write(c: &mut Committer<Crash, hsearch_commit::AnyDependencies>, batches: &[Vec<u8>]) -> u64 {
    let mut last = 0;
    for (seq, b) in batches.iter().enumerate() {
        let r: Receipt = commit(
            c,
            Request {
                body: b.clone(),
                ..req(INDEX, seq as u64, "")
            },
        );
        last = r.ordinal;
    }
    last
}

/// The outcome each step must report, from a model of live identities that never uses the engine.
fn outcomes(history: &[Vec<Step>]) -> Vec<Vec<Outcome>> {
    let mut live = std::collections::BTreeSet::new();
    history
        .iter()
        .map(|batch| {
            batch
                .iter()
                .map(|s| match s.tag {
                    Some(_) if live.insert(s.id) => Outcome::Created,
                    Some(_) => Outcome::Updated,
                    None if live.remove(s.id) => Outcome::Deleted,
                    None => Outcome::NotFound,
                })
                .collect()
        })
        .collect()
}

/// The last step per identity: its version and tag, `None` tag for a delete.
fn ledger(history: &[Vec<Step>]) -> BTreeMap<&'static str, (i64, Option<&'static str>)> {
    let mut live = BTreeMap::new();
    for step in history.iter().flatten() {
        live.insert(step.id, (step.version, step.tag));
    }
    live
}

#[test]
fn a_rebuilt_engine_holds_what_the_ledger_says() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let h = history();
    let bodies: Vec<Vec<u8>> = h.iter().enumerate().map(|(i, s)| body(i, s)).collect();
    let last = write(&mut c, &bodies);
    drop(c);
    let schema = schema();
    let dir = std::env::temp_dir().join(format!("hsearch-rebuild-{}", std::process::id()));
    hsearch_engine::with_local_engine(&dir, schema.clone(), |engine, _| {
        let p = publisher(&store);
        let rebuilt = rebuild(
            &p,
            &target(
                &lane,
                big(),
                Expect {
                    ordinal: last + 1,
                    seq: h.len() as u64,
                },
            ),
            &AnyDependencies,
            &schema,
            engine,
        )
        .unwrap();
        assert_eq!(rebuilt.recovered.applied, h.len() as u64);
        assert_eq!(rebuilt.receipts.len(), h.len());
        // Every batch's receipts are the ledger's: outcome, version and removal, in order.
        let model = outcomes(&h);
        for (i, (seq, receipts)) in rebuilt.receipts.iter().enumerate() {
            assert_eq!(*seq, i as u64);
            let got: Vec<_> = receipts
                .iter()
                .map(|r| (r.id.as_str(), r.version, r.outcome))
                .collect();
            let want: Vec<_> = h[i]
                .iter()
                .zip(&model[i])
                .map(|(s, o)| (s.id, s.version, *o))
                .collect();
            assert_eq!(got, want, "batch {i}");
        }
        let snapshot = engine.refresh().unwrap();
        for (id, (version, tag)) in ledger(&h) {
            let got = snapshot.get(id, &["tag"]).unwrap();
            match tag {
                None => assert!(got.is_none(), "{id}"),
                Some(tag) => {
                    let hit = got.unwrap();
                    assert_eq!(hit.version, version, "{id}");
                    assert_eq!(
                        hit.fields,
                        vec![("tag".to_string(), StoredValue::String(tag.to_string()))],
                        "{id}"
                    );
                }
            }
        }
        // Versions: a retry at a version the log already holds is refused, one above it is not.
        let stale = [Mutation::Delete {
            id: "a",
            version: 2,
        }];
        assert!(
            engine
                .apply(schema.prepare("later", &stale).unwrap())
                .is_err()
        );
        let newer = [Mutation::Delete {
            id: "a",
            version: 6,
        }];
        assert_eq!(
            engine
                .apply(schema.prepare("later2", &newer).unwrap())
                .unwrap()
                .len(),
            1
        );
        // The first batch's request identity is remembered: replaying it is recognised, not
        // applied twice.
        let first = engine
            .apply(
                schema
                    .prepare(
                        "r0",
                        &[
                            Mutation::Put {
                                id: "a",
                                version: 1,
                                fields: &[("tag", Value::String("x"))],
                            },
                            Mutation::Put {
                                id: "b",
                                version: 1,
                                fields: &[("tag", Value::String("y"))],
                            },
                        ],
                    )
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(first, rebuilt.receipts[0].1);
    })
    .unwrap();
}

#[test]
fn a_body_the_engine_refuses_stops_recovery_at_its_sequence() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let h = history();
    let mut bodies: Vec<Vec<u8>> = h.iter().enumerate().map(|(i, s)| body(i, s)).collect();
    bodies[2] = vec![0xff, 0xff];
    write(&mut c, &bodies);
    drop(c);
    let schema = schema();
    let dir = std::env::temp_dir().join(format!("hsearch-rebuild-bad-{}", std::process::id()));
    hsearch_engine::with_local_engine(&dir, schema.clone(), |engine, _| {
        let p = publisher(&store);
        let err = rebuild(
            &p,
            &target(&lane, big(), Expect::default()),
            &AnyDependencies,
            &schema,
            engine,
        )
        .unwrap_err();
        assert_eq!(
            err,
            RebuildError::Engine {
                seq: 2,
                error: EngineError::Document
            }
        );
    })
    .unwrap();
}

#[test]
fn a_record_level_delete_and_a_not_ready_log_are_told_apart() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    commit(&mut c, req(INDEX, 0, "x"));
    commit(&mut c, del(INDEX, 1));
    drop(c);
    let schema = schema();
    let dir = std::env::temp_dir().join(format!("hsearch-rebuild-del-{}", std::process::id()));
    hsearch_engine::with_local_engine(&dir, schema.clone(), |engine, _| {
        let p = publisher(&store);
        // Seq 0's body is not a batch: the engine refuses it first.
        let first = rebuild(
            &p,
            &target(&lane, big(), Expect::default()),
            &AnyDependencies,
            &schema,
            engine,
        )
        .unwrap_err();
        assert!(matches!(first, RebuildError::Engine { seq: 0, .. }));
    })
    .unwrap();
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let b = body(0, &history()[0]);
    commit(
        &mut c,
        Request {
            body: b,
            ..req(INDEX, 0, "")
        },
    );
    commit(&mut c, del(INDEX, 1));
    drop(c);
    hsearch_engine::with_local_engine(&dir, schema.clone(), |engine, _| {
        let p = publisher(&store);
        assert_eq!(
            rebuild(
                &p,
                &target(&lane, big(), Expect::default()),
                &AnyDependencies,
                &schema,
                engine
            )
            .unwrap_err(),
            RebuildError::Unsupported(1)
        );
        // No head at all is recovery's refusal, passed through.
        let empty = Crash::new();
        let p = publisher(&empty);
        assert_eq!(
            rebuild(
                &p,
                &target(&lane, big(), Expect::default()),
                &AnyDependencies,
                &schema,
                engine
            )
            .unwrap_err(),
            RebuildError::NotReady(NotReady::NoHead)
        );
    })
    .unwrap();
}

#[test]
fn a_head_that_names_a_base_is_refused_not_served_short() {
    let store = Crash::new();
    let (lane, mut c) = open_lane(&store);
    let h = history();
    let bodies: Vec<Vec<u8>> = h.iter().enumerate().map(|(i, s)| body(i, s)).collect();
    write(&mut c, &bodies);
    drop(c);
    let schema = schema();
    with_commit("rebuild-base", |local, _, names| {
        let p = publisher(&store);
        let s = spec(cut(2), 5, 1);
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        let (head, version) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
        p.activate(
            &at(&lane, &head, &version),
            &published,
            &s,
            2,
            &AnyDependencies,
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("hsearch-rebuild-base-{}", std::process::id()));
        hsearch_engine::with_local_engine(&dir, schema.clone(), |engine, _| {
            assert_eq!(
                rebuild(
                    &publisher(&store),
                    &target(&lane, big(), Expect::default()),
                    &AnyDependencies,
                    &schema,
                    engine
                )
                .unwrap_err(),
                RebuildError::BaseNotOpened
            );
        })
        .unwrap();
    });
}
