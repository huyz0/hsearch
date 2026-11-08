//! Native write/commit/refresh/merge faults and supported writer ownership.
use hsearch_engine::{Engine, Error, Kind, Mutation, Query, Schema, Value};
use lucene_store::ByteBuffersDirectory;
use std::sync::atomic::Ordering;
#[path = "support/adapter_directory.rs"]
mod support;
use support::FaultDirectory;

#[test]
fn failed_native_mutation_commit_refresh_and_merge_poison_continuation() {
    for action in ["apply", "commit-sync", "commit-rename", "refresh", "merge"] {
        let directory = FaultDirectory::new();
        let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
        let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
        let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
        engine
            .apply(
                schema
                    .prepare(
                        "initial",
                        &[Mutation::Put {
                            id: "stable",
                            version: 1,
                            fields: &[("key", Value::String("old"))],
                        }],
                    )
                    .unwrap(),
            )
            .unwrap();
        let checkpoint = engine.commit().unwrap();
        let old = engine.refresh().unwrap();
        if action == "merge" {
            for i in 0..8 {
                let id = format!("extra-{i}");
                engine
                    .apply(
                        schema
                            .prepare(
                                &id,
                                &[Mutation::Put {
                                    id: &id,
                                    version: 1,
                                    fields: &[("key", Value::String("new"))],
                                }],
                            )
                            .unwrap(),
                    )
                    .unwrap();
            }
            engine.commit().unwrap();
        }
        directory.mode.store(
            match action {
                "commit-sync" => 2,
                "commit-rename" => 3,
                _ => 1,
            },
            Ordering::SeqCst,
        );
        let result = match action {
            "apply" => {
                let ids: Vec<_> = (0..8).map(|i| format!("new-{i}")).collect();
                let fields = [("key", Value::String("new"))];
                let operations: Vec<_> = ids
                    .iter()
                    .map(|id| Mutation::Put {
                        id,
                        version: 1,
                        fields: &fields,
                    })
                    .collect();
                engine
                    .apply(schema.prepare("failed-batch", &operations).unwrap())
                    .map(|_| ())
            }
            "commit-sync" | "commit-rename" => {
                engine
                    .apply(
                        schema
                            .prepare(
                                "pending",
                                &[Mutation::Put {
                                    id: "stable",
                                    version: 2,
                                    fields: &[("key", Value::String("new"))],
                                }],
                            )
                            .unwrap(),
                    )
                    .unwrap();
                engine.commit().map(|_| ())
            }
            "refresh" => {
                engine
                    .apply(
                        schema
                            .prepare(
                                "pending",
                                &[Mutation::Put {
                                    id: "stable",
                                    version: 2,
                                    fields: &[("key", Value::String("new"))],
                                }],
                            )
                            .unwrap(),
                    )
                    .unwrap();
                engine.refresh().map(|_| ())
            }
            "merge" => engine.merge(),
            _ => unreachable!(),
        };
        assert_eq!(result, Err(Error::Native), "{action}");
        assert!(matches!(engine.refresh(), Err(Error::Poisoned)), "{action}");
        assert!(matches!(engine.commit(), Err(Error::Poisoned)), "{action}");
        assert_eq!(
            engine.apply(
                schema
                    .prepare(
                        "later",
                        &[Mutation::Delete {
                            id: "stable",
                            version: 3
                        }]
                    )
                    .unwrap()
            ),
            Err(Error::Poisoned),
            "{action}"
        );
        assert_eq!(
            old.search(&Query::All, &[], 128, &[]).unwrap().total,
            1,
            "{action}"
        );
        directory.mode.store(0, Ordering::SeqCst);
        engine.close().unwrap();
        assert!(
            matches!(
                Engine::open(&observed_directory, schema, Some(checkpoint)),
                Err(Error::Checkpoint)
            ),
            "{action}"
        );
    }
}

#[test]
fn a_second_writer_is_refused_and_checkpoint_schema_is_not_interchangeable() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = Schema::new(1, &[("key", Kind::Keyword)]).unwrap();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    // Even an empty index has one native write-lock owner.
    assert!(Engine::open(&observed_directory, schema.clone(), None).is_err());
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    let changed = Schema::new(2, &[("key", Kind::Keyword)]).unwrap();
    assert!(matches!(
        Engine::open(&observed_directory, changed, Some(checkpoint.clone())),
        Err(Error::Checkpoint)
    ));
    let empty = ByteBuffersDirectory::new();
    let observed_empty = hsearch_engine::EngineDirectory::new(&empty);
    assert!(matches!(
        Engine::open(&observed_empty, schema, Some(checkpoint)),
        Err(Error::Checkpoint)
    ));
}
