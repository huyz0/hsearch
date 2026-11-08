//! Native CRUD, lineage, missing-delete and deterministic admission controls.
use hsearch_engine::{Engine, Error, Kind, Mutation, Outcome, Query, Schema, Value};
use lucene_store::ByteBuffersDirectory;
use std::sync::{atomic::Ordering, mpsc::sync_channel};
use std::time::Duration;
#[path = "support/adapter_directory.rs"]
mod support;
use support::{Block, FaultDirectory};

fn schema() -> Schema {
    Schema::new(1, &[("key", Kind::Keyword)]).unwrap()
}
fn put(
    schema: &Schema,
    request: &str,
    id: &str,
    version: i64,
    key: &str,
) -> hsearch_engine::PreparedBatch {
    schema
        .prepare(
            request,
            &[Mutation::Put {
                id,
                version,
                fields: &[("key", Value::String(key))],
            }],
        )
        .unwrap()
}
#[test]
fn stale_same_projection_checkpoint_cannot_forget_a_missing_delete_or_retry() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine
        .apply(put(&schema, "initial", "stable", 1, "same"))
        .unwrap();
    let a = engine.commit().unwrap();
    let deletion = engine
        .apply(
            schema
                .prepare(
                    "missing-delete",
                    &[Mutation::Delete {
                        id: "missing",
                        version: 10,
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    assert_eq!(deletion[0].outcome, Outcome::NotFound);
    assert!(!deletion[0].removed);
    let b = engine.commit().unwrap();
    engine.close().unwrap();
    assert!(matches!(
        Engine::open(&observed_directory, schema.clone(), Some(a)),
        Err(Error::Checkpoint)
    ));
    let engine = Engine::open(&observed_directory, schema.clone(), Some(b.clone())).unwrap();
    assert_eq!(
        engine
            .apply(
                schema
                    .prepare(
                        "missing-delete",
                        &[Mutation::Delete {
                            id: "missing",
                            version: 10
                        }]
                    )
                    .unwrap()
            )
            .unwrap(),
        deletion
    );
    assert_eq!(
        engine.apply(put(&schema, "old-version", "missing", 9, "wrong")),
        Err(Error::Conflict)
    );
    assert_eq!(
        engine.apply(put(
            &schema,
            "missing-delete",
            "missing",
            10,
            "different opcode"
        )),
        Err(Error::Conflict)
    );
    let newest = engine.commit().unwrap();
    engine.close().unwrap();
    assert!(matches!(
        Engine::open(&observed_directory, schema.clone(), Some(b)),
        Err(Error::Checkpoint)
    ));
    Engine::open(&observed_directory, schema, Some(newest))
        .unwrap()
        .close()
        .unwrap();
}
#[test]
fn native_create_point_read_and_changed_then_restored_history_keep_original_results() {
    let directory = ByteBuffersDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    let create = || {
        schema
            .prepare(
                "create",
                &[Mutation::Create {
                    id: "id",
                    version: 1,
                    fields: &[("key", Value::String("original"))],
                }],
            )
            .unwrap()
    };
    let receipt = engine.apply(create()).unwrap();
    assert_eq!(receipt[0].outcome, Outcome::Created);
    let a = engine.commit().unwrap();
    assert_eq!(
        engine.apply(
            schema
                .prepare(
                    "conflicting-create",
                    &[Mutation::Create {
                        id: "id",
                        version: 2,
                        fields: &[("key", Value::String("other"))]
                    }]
                )
                .unwrap()
        ),
        Err(Error::Conflict)
    );
    let updated = engine
        .apply(put(&schema, "replace", "id", 2, "changed"))
        .unwrap();
    assert_eq!(updated[0].outcome, Outcome::Updated);
    let removed = engine
        .apply(
            schema
                .prepare(
                    "delete",
                    &[Mutation::Delete {
                        id: "id",
                        version: 3,
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    assert_eq!(removed[0].outcome, Outcome::Deleted);
    assert!(engine.refresh().unwrap().get("id", &[]).unwrap().is_none());
    engine
        .apply(
            schema
                .prepare(
                    "recreate",
                    &[Mutation::Create {
                        id: "id",
                        version: 4,
                        fields: &[("key", Value::String("original"))],
                    }],
                )
                .unwrap(),
        )
        .unwrap();
    let b = engine.commit().unwrap();
    engine.close().unwrap();
    assert!(matches!(
        Engine::open(&observed_directory, schema.clone(), Some(a)),
        Err(Error::Checkpoint)
    ));
    let engine = Engine::open(&observed_directory, schema.clone(), Some(b)).unwrap();
    let hit = engine
        .refresh()
        .unwrap()
        .get("id", &["key"])
        .unwrap()
        .unwrap();
    assert_eq!(hit.id, "id");
    assert_eq!(hit.version, 4);
    assert_eq!(engine.apply(create()).unwrap(), receipt);
    assert_eq!(engine.refresh().unwrap().get("", &[]), Err(Error::Document));
    engine.close().unwrap();
}
#[test]
fn blocked_native_commit_refuses_competing_admission_and_reopen_without_queueing() {
    let directory = FaultDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine
        .apply(put(&schema, "initial", "id", 1, "old"))
        .unwrap();
    let checkpoint = engine.commit().unwrap();
    let old = engine.refresh().unwrap();
    engine
        .apply(put(&schema, "pending", "id", 2, "new"))
        .unwrap();
    let (entered_tx, entered_rx) = sync_channel(1);
    let (resume_tx, resume_rx) = sync_channel(1);
    *directory.block.lock().unwrap() = Some(Block {
        entered: entered_tx,
        resume: resume_rx,
    });
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| engine.commit());
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            engine.apply(put(&schema, "refused", "other", 1, "private")),
            Err(Error::Busy)
        );
        assert!(matches!(engine.refresh(), Err(Error::Busy)));
        assert!(matches!(engine.commit(), Err(Error::Busy)));
        assert_eq!(engine.merge(), Err(Error::Busy));
        assert!(matches!(
            Engine::open(&observed_directory, schema.clone(), Some(checkpoint)),
            Err(Error::Busy)
        ));
        assert_eq!(old.get("id", &[]).unwrap().unwrap().version, 1);
        resume_tx.send(()).unwrap();
        writer.join().unwrap().unwrap();
    });
    assert_eq!(
        engine
            .refresh()
            .unwrap()
            .search(&Query::All, &[], 128, &[])
            .unwrap()
            .total,
        1
    );
    engine.close().unwrap();
}
#[test]
fn implicit_drop_and_failed_native_close_leave_checkpoint_lineage_uncertain() {
    for fail_close in [false, true] {
        let directory = FaultDirectory::new();
        let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
        let schema = schema();
        let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
        engine
            .apply(put(&schema, "initial", "id", 1, "old"))
            .unwrap();
        let checkpoint = engine.commit().unwrap();
        let old = engine.refresh().unwrap();
        assert_eq!(directory.failures.load(Ordering::SeqCst), 0);
        if fail_close {
            // Pinned lifecycle.rs:73-81 propagates try_rollback errors;
            // index_file_deleter.rs:603-604 calls Directory::list_all.
            // The observing directory separately records actual lock release.
            directory.mode.store(5, Ordering::SeqCst);
            assert_eq!(engine.close(), Err(Error::Native));
            assert_eq!(directory.failures.load(Ordering::SeqCst), 1);
            directory.mode.store(0, Ordering::SeqCst);
        } else {
            drop(engine);
            assert_eq!(directory.failures.load(Ordering::SeqCst), 0);
        }
        // The directory is healthy again, so refusal must come from the
        // uncertain lineage rather than from a still-armed I/O fault.
        assert!(matches!(
            Engine::open(&observed_directory, schema, Some(checkpoint)),
            Err(Error::Checkpoint)
        ));
        assert_eq!(old.get("id", &[]).unwrap().unwrap().version, 1);
    }
}

// The observing lock delegates the actual acquired Lock::close on native
// writer destruction; these controls require execution at the unchanged pin.
#[test]
fn actual_lock_release_failure_is_returned_and_invalidates_lineage() {
    let directory = FaultDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine
        .apply(put(&schema, "initial", "id", 1, "old"))
        .unwrap();
    let checkpoint = engine.commit().unwrap();
    let old = engine.refresh().unwrap();
    assert_eq!(directory.lock_closes.load(Ordering::SeqCst), 0);
    directory.mode.store(6, Ordering::SeqCst);
    let result = engine.close();
    // Count the actual Lock::close boundary before inspecting its result.
    assert_eq!(directory.lock_closes.load(Ordering::SeqCst), 1);
    assert_eq!(directory.failures.load(Ordering::SeqCst), 1);
    assert_eq!(
        result,
        Err(Error::Close {
            cleanup: hsearch_engine::CleanupStatus::Succeeded,
            release: hsearch_engine::ReleaseStatus::Failed,
        })
    );
    directory.mode.store(0, Ordering::SeqCst);
    assert!(matches!(
        Engine::open(&observed_directory, schema, Some(checkpoint)),
        Err(Error::Checkpoint)
    ));
    assert_eq!(old.get("id", &[]).unwrap().unwrap().version, 1);
}

#[test]
fn reusable_close_requires_one_observed_lock_release() {
    let directory = FaultDirectory::new();
    let observed_directory = hsearch_engine::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    let checkpoint = engine.commit().unwrap();
    assert_eq!(directory.lock_closes.load(Ordering::SeqCst), 0);
    engine.close().unwrap();
    assert_eq!(directory.lock_closes.load(Ordering::SeqCst), 1);
    let reopened = Engine::open(&observed_directory, schema, Some(checkpoint)).unwrap();
    reopened.close().unwrap();
    assert_eq!(directory.lock_closes.load(Ordering::SeqCst), 2);
}
