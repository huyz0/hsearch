//! Production lifecycle, private preparation and finite work controls.
use hsearch_engine::{
    Engine, Error, Kind, Mutation, Query, Schema, Snapshot, Sort, StoredValue, Value,
};
use lucene_store::ByteBuffersDirectory;
use std::sync::{Barrier, mpsc::sync_channel};

fn schema() -> Schema {
    Schema::new(7, &[("title", Kind::Text), ("price", Kind::Integer)]).unwrap()
}
fn put(
    schema: &Schema,
    request: &str,
    id: &str,
    version: i64,
    title: &str,
) -> hsearch_engine::PreparedBatch {
    schema
        .prepare(
            request,
            &[Mutation::Put {
                id,
                version,
                fields: &[
                    ("title", Value::String(title)),
                    ("price", Value::Integer(10)),
                ],
            }],
        )
        .unwrap()
}
fn state(snapshot: &Snapshot) -> Vec<(String, i64, String)> {
    snapshot
        .search(&Query::All, &[], 128, &["title"])
        .unwrap()
        .hits
        .into_iter()
        .map(|hit| {
            let StoredValue::String(title) = &hit.fields[0].1 else {
                panic!("title type")
            };
            (hit.id, hit.version, title.clone())
        })
        .collect()
}

#[test]
fn reopen_replay_preserves_identity_payload_receipts_and_rollback() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    let first = engine
        .apply(put(&schema, "request-1", "stable", 1, "original"))
        .unwrap();
    let old = engine.refresh().unwrap();
    engine
        .apply(put(&schema, "request-2", "stable", 3, "replacement"))
        .unwrap();
    engine
        .apply(put(&schema, "request-3", "other", 2, "remove"))
        .unwrap();
    let deletion = engine
        .apply(
            schema
                .prepare(
                    "request-4",
                    &[Mutation::Delete {
                        id: "other",
                        version: 4,
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    assert!(deletion[0].removed);
    assert_eq!(state(&old), vec![("stable".into(), 1, "original".into())]);
    engine.merge().unwrap();
    let checkpoint = engine.commit().unwrap();
    engine
        .apply(put(&schema, "pending", "stable", 5, "must roll back"))
        .unwrap();
    engine.close().unwrap();
    assert!(matches!(
        Engine::open(&observed_directory, schema.clone(), None),
        Err(Error::Checkpoint)
    ));
    let engine = Engine::open(
        &observed_directory,
        schema.clone(),
        Some(checkpoint.clone()),
    )
    .unwrap();
    assert_eq!(
        state(&engine.refresh().unwrap()),
        vec![("stable".into(), 3, "replacement".into())]
    );
    assert_eq!(
        engine
            .apply(put(&schema, "request-1", "stable", 1, "original"))
            .unwrap(),
        first
    );
    assert_eq!(
        engine
            .apply(
                schema
                    .prepare(
                        "request-4",
                        &[Mutation::Delete {
                            id: "other",
                            version: 4
                        }]
                    )
                    .unwrap()
            )
            .unwrap(),
        deletion
    );
    assert_eq!(
        engine.apply(put(&schema, "request-1", "stable", 1, "different")),
        Err(Error::Conflict)
    );
    assert_eq!(
        engine.apply(put(&schema, "new-request", "stable", 3, "stale")),
        Err(Error::Conflict)
    );
    engine.close().unwrap();
    let replay_directory = ByteBuffersDirectory::new();
    let observed_replay_directory = hsearch_engine::EngineDirectory::new(&replay_directory);
    let replay = Engine::open(&observed_replay_directory, schema.clone(), None).unwrap();
    assert_eq!(
        replay
            .apply(put(&schema, "request-1", "stable", 1, "original"))
            .unwrap(),
        first
    );
    replay
        .apply(put(&schema, "request-2", "stable", 3, "replacement"))
        .unwrap();
    replay
        .apply(put(&schema, "request-3", "other", 2, "remove"))
        .unwrap();
    assert_eq!(
        replay
            .apply(
                schema
                    .prepare(
                        "request-4",
                        &[Mutation::Delete {
                            id: "other",
                            version: 4
                        }]
                    )
                    .unwrap()
            )
            .unwrap(),
        deletion
    );
    assert_eq!(
        state(&replay.refresh().unwrap()),
        vec![("stable".into(), 3, "replacement".into())]
    );
    replay.close().unwrap();
    let foreign = ByteBuffersDirectory::new();
    let observed_foreign = hsearch_engine::EngineDirectory::new(&foreign);
    let wrong = Engine::open(&observed_foreign, schema.clone(), None).unwrap();
    wrong
        .apply(put(&schema, "wrong", "stable", 3, "changed payload"))
        .unwrap();
    wrong.commit().unwrap();
    wrong.close().unwrap();
    assert!(matches!(
        Engine::open(&observed_foreign, schema, Some(checkpoint)),
        Err(Error::Checkpoint)
    ));
}

#[test]
fn private_invalid_middle_and_valid_refusal_during_concurrent_refresh() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine
        .apply(put(&schema, "initial", "stable", 1, "committed"))
        .unwrap();
    engine.commit().unwrap();
    let barrier = Barrier::new(2);
    let (prepared_tx, prepared_rx) = sync_channel(1);
    let (seen_tx, seen_rx) = sync_channel(1);
    std::thread::scope(|scope| {
        let engine = &engine;
        let schema = &schema;
        let barrier = &barrier;
        scope.spawn(move || {
            barrier.wait();
            let valid = [
                ("title", Value::String("must stay private")),
                ("price", Value::Integer(20)),
            ];
            let invalid = [
                ("title", Value::String("bad tail")),
                ("price", Value::Integer(i64::MAX)),
            ];
            let failed = schema.prepare(
                "invalid",
                &[
                    Mutation::Put {
                        id: "stable",
                        version: 2,
                        fields: &valid,
                    },
                    Mutation::Put {
                        id: "bad",
                        version: 1,
                        fields: &invalid,
                    },
                ],
            );
            assert!(matches!(failed, Err(Error::Document)));
            prepared_tx.send(1).unwrap();
            seen_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let refused = put(schema, "valid-but-refused", "stable", 2, "refused");
            prepared_tx.send(2).unwrap();
            seen_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            drop(refused);
            engine
                .apply(put(schema, "accepted", "stable", 2, "accepted"))
                .unwrap();
        });
        barrier.wait();
        for stage in [1, 2] {
            assert_eq!(
                prepared_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap(),
                stage
            );
            for _ in 0..8 {
                assert_eq!(
                    state(&engine.refresh().unwrap()),
                    vec![("stable".into(), 1, "committed".into())]
                );
            }
            seen_tx.send(()).unwrap();
        }
    });
    assert_eq!(
        state(&engine.refresh().unwrap()),
        vec![("stable".into(), 2, "accepted".into())]
    );
    engine.close().unwrap();
}

#[test]
fn bounded_schema_document_query_sort_projection_and_bucket_refusals() {
    for fields in [
        vec![],
        vec![("bad.name", Kind::Text)],
        vec![("same", Kind::Text), ("same", Kind::Integer)],
        vec![("_hs_id", Kind::Keyword)],
    ] {
        assert_eq!(Schema::new(1, &fields), Err(Error::Schema));
    }
    assert_eq!(Schema::new(0, &[("ok", Kind::Text)]), Err(Error::Schema));
    let schema = schema();
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    let too_big = "x".repeat(8193);
    let too_many = "word ".repeat(129);
    for text in [&too_big, &too_many] {
        assert!(matches!(
            schema.prepare(
                "bad",
                &[Mutation::Put {
                    id: "stable",
                    version: 1,
                    fields: &[("title", Value::String(text)), ("price", Value::Integer(1))]
                }]
            ),
            Err(Error::Limit)
        ));
    }
    for fields in [
        vec![("title", Value::String("ok"))],
        vec![
            ("title", Value::String("ok")),
            ("price", Value::String("10")),
        ],
        vec![
            ("title", Value::String("ok")),
            ("unknown", Value::Integer(10)),
        ],
    ] {
        assert!(matches!(
            schema.prepare(
                "bad",
                &[Mutation::Put {
                    id: "stable",
                    version: 1,
                    fields: &fields
                }]
            ),
            Err(Error::Document)
        ));
    }
    assert!(matches!(
        schema.prepare("bad", &[Mutation::Delete { id: "", version: 1 }]),
        Err(Error::Document)
    ));
    assert!(matches!(
        schema.prepare(
            "bad",
            &[
                Mutation::Delete {
                    id: "same",
                    version: 1
                },
                Mutation::Delete {
                    id: "same",
                    version: 2
                }
            ]
        ),
        Err(Error::Conflict)
    ));
    engine
        .apply(put(&schema, "ok", "stable", 1, "brown fox"))
        .unwrap();
    let snapshot = engine.refresh().unwrap();
    assert_eq!(
        snapshot.search(&Query::Unsupported, &[], 1, &[]),
        Err(Error::Unsupported)
    );
    assert_eq!(
        snapshot.search(&Query::All, &[], 129, &[]),
        Err(Error::Limit)
    );
    assert_eq!(
        snapshot.search(&Query::All, &[], 1, &["title", "title"]),
        Err(Error::Document)
    );
    assert_eq!(
        snapshot.search(&Query::All, &[], 1, &["unknown"]),
        Err(Error::Schema)
    );
    assert_eq!(
        snapshot.search(
            &Query::All,
            &[Sort::Field {
                name: "title",
                descending: false
            }],
            1,
            &[]
        ),
        Err(Error::Unsupported)
    );
    assert_eq!(
        snapshot.search(
            &Query::Term {
                field: "price",
                value: Value::String("10")
            },
            &[],
            1,
            &[]
        ),
        Err(Error::Document)
    );
    assert_eq!(
        snapshot.metrics(&Query::All, &["title"]),
        Err(Error::Unsupported)
    );
    assert_eq!(
        snapshot.groups(&Query::All, "title", 1),
        Err(Error::Unsupported)
    );
    assert_eq!(snapshot.search(&Query::All, &[], 0, &[]).unwrap().total, 1);
    assert!(
        snapshot
            .search(&Query::All, &[], 0, &[])
            .unwrap()
            .hits
            .is_empty()
    );
    let none = Query::Term {
        field: "title",
        value: Value::String("absent"),
    };
    assert_eq!(snapshot.metrics(&none, &["price"]).unwrap()[0].min, None);
    assert_eq!(snapshot.metrics(&none, &["price"]).unwrap()[0].count, 0);
    engine.close().unwrap();
}

#[test]
fn exact_groups_refuse_cardinality_overflow_and_stress_has_a_finite_end() {
    let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    for i in 0..128 {
        let key = format!("{i:03}");
        engine
            .apply(
                schema
                    .prepare(
                        &key,
                        &[Mutation::Put {
                            id: &key,
                            version: 1,
                            fields: &[("key", Value::String(&key))],
                        }],
                    )
                    .unwrap(),
            )
            .unwrap();
    }
    let snapshot = engine.refresh().unwrap();
    assert_eq!(snapshot.groups(&Query::All, "key", 127), Err(Error::Limit));
    let groups = snapshot.groups(&Query::All, "key", 128).unwrap();
    assert_eq!(groups.len(), 128);
    assert!(groups.iter().all(|b| b.count == 1));
    assert_eq!(groups[0].key, "000");
    assert_eq!(
        engine.apply(
            schema
                .prepare(
                    "overflow",
                    &[Mutation::Put {
                        id: "new",
                        version: 1,
                        fields: &[("key", Value::String("new"))]
                    }]
                )
                .unwrap()
        ),
        Err(Error::Limit)
    );
    for i in 0..128 {
        let key = format!("{i:03}");
        engine
            .apply(
                schema
                    .prepare(
                        &format!("delete-{key}"),
                        &[Mutation::Delete {
                            id: &key,
                            version: 2,
                        }],
                    )
                    .unwrap(),
            )
            .unwrap();
    }
    assert_eq!(
        engine
            .refresh()
            .unwrap()
            .search(&Query::All, &[], 128, &[])
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        engine.apply(
            schema
                .prepare(
                    "operation-overflow",
                    &[Mutation::Delete {
                        id: "000",
                        version: 3
                    }]
                )
                .unwrap()
        ),
        Err(Error::Limit)
    );
    assert_eq!(
        snapshot.search(&Query::All, &[], 128, &[]).unwrap().total,
        128
    );
    engine.close().unwrap();
}
