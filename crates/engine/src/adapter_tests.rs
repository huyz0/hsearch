//! Private boundary controls use real native commits, then inject read failure.
use super::*;
use crate::MAX_TERMS;
use crate::adapter_test_support as support;
use std::sync::atomic::Ordering;
use support::FaultDirectory;

fn schema() -> Schema {
    Schema::new(1, &[("key", crate::Kind::Keyword)]).unwrap()
}
fn batch(schema: &Schema, version: i64) -> PreparedBatch {
    schema
        .prepare(
            &format!("request-{version}"),
            &[Mutation::Put {
                id: "id",
                version,
                fields: &[("key", Value::String("value"))],
            }],
        )
        .unwrap()
}
#[test]
fn successful_native_commit_then_identity_read_failure_invalidates_old_checkpoints() {
    let directory = FaultDirectory::new();
    let observed_directory = crate::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    engine.apply(batch(&schema, 1)).unwrap();
    let old_checkpoint = engine.commit().unwrap();
    let old = engine.refresh().unwrap();
    engine.apply(batch(&schema, 2)).unwrap();
    {
        let mut state = engine.state.lock().unwrap();
        let mut head = engine.lease.lineage.head.lock().unwrap();
        engine.writer.lock().unwrap().commit().unwrap();
        let epoch = head.epoch + 1;
        directory.mode.store(4, Ordering::SeqCst);
        assert!(matches!(
            engine.complete_checkpoint(&mut state, &mut head, epoch),
            Err(Error::Native)
        ));
        assert!(head.uncertain);
        assert!(state.poisoned);
    }
    directory.mode.store(0, Ordering::SeqCst);
    assert!(matches!(engine.commit(), Err(Error::Poisoned)));
    assert!(matches!(engine.refresh(), Err(Error::Poisoned)));
    assert_eq!(old.get("id", &[]).unwrap().unwrap().version, 1);
    engine.close().unwrap();
    assert!(matches!(
        Engine::open(&observed_directory, schema, Some(old_checkpoint)),
        Err(Error::Checkpoint)
    ));
}
#[test]
fn epoch_exhaustion_refuses_before_native_commit_or_checkpoint_invalidation() {
    let directory = FaultDirectory::new();
    let observed_directory = crate::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema, None).unwrap();
    engine.commit().unwrap();
    let native = NativeCommit::read(&directory).unwrap();
    engine.lease.lineage.head.lock().unwrap().epoch = u64::MAX;
    assert!(matches!(engine.commit(), Err(Error::Limit)));
    assert_eq!(NativeCommit::read(&directory).unwrap(), native);
    assert!(!engine.lease.lineage.head.lock().unwrap().uncertain);
    engine.close().unwrap();
}
#[test]
fn equal_native_descriptor_does_not_authorize_an_older_history_epoch() {
    let directory = FaultDirectory::new();
    let observed_directory = crate::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    let old = engine.commit().unwrap();
    let current = engine.commit().unwrap();
    engine.close().unwrap();
    // Deliberately alias the old descriptor to the current one. This isolates
    // epoch discrimination from whether the native writer advances on a no-op.
    let mut aliased = old;
    aliased.native = current.native;
    assert!(matches!(
        Engine::open(&observed_directory, schema.clone(), Some(aliased)),
        Err(Error::Checkpoint)
    ));
    Engine::open(&observed_directory, schema, Some(current))
        .unwrap()
        .close()
        .unwrap();
}
#[test]
fn poisoned_mutexes_and_redacted_errors_have_honest_continuation() {
    let directory = FaultDirectory::new();
    let observed_directory = crate::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _state = engine.state.lock().unwrap();
        panic!("injected admission unwind");
    }));
    assert!(result.is_err());
    assert_eq!(engine.apply(batch(&schema, 1)), Err(Error::Poisoned));
    assert!(matches!(engine.refresh(), Err(Error::Poisoned)));
    assert!(matches!(engine.commit(), Err(Error::Poisoned)));
    assert_eq!(engine.merge(), Err(Error::Poisoned));
    engine.close().unwrap();
    let directory = FaultDirectory::new();
    let observed_directory = crate::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    let checkpoint = engine.commit().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _writer = engine.writer.lock().unwrap();
        panic!("injected writer unwind");
    }));
    assert!(result.is_err());
    assert_eq!(engine.close(), Err(Error::Poisoned));
    assert!(matches!(
        Engine::open(&observed_directory, schema, Some(checkpoint)),
        Err(Error::Checkpoint)
    ));
    assert_eq!(Error::Native.to_string(), "engine Native");
    assert_eq!(Error::Poisoned.to_string(), "engine Poisoned");
}
#[test]
fn failed_reopen_io_has_no_writer_or_lineage_effects() {
    let directory = FaultDirectory::new();
    let observed_directory = crate::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed_directory, schema.clone(), None).unwrap();
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    for mode in [4, 5] {
        directory.mode.store(mode, Ordering::SeqCst);
        assert!(matches!(
            Engine::open(
                &observed_directory,
                schema.clone(),
                Some(checkpoint.clone())
            ),
            Err(Error::Native)
        ));
    }
    directory.mode.store(0, Ordering::SeqCst);
    Engine::open(&observed_directory, schema, Some(checkpoint))
        .unwrap()
        .close()
        .unwrap();
}

#[test]
fn acquired_constructor_and_configuration_failures_guard_checkpoint_lineage() {
    // Real native constructor acquires first, then calls list_all. mode8 fails
    // that read; mode10 also fails the actual acquired Lock::close.
    for mode in [7, 8, 10] {
        let directory = FaultDirectory::new();
        let observed = crate::EngineDirectory::new(&directory);
        let schema = schema();
        let engine = Engine::open(&observed, schema.clone(), None).unwrap();
        engine.apply(batch(&schema, 1)).unwrap();
        let checkpoint = engine.commit().unwrap();
        let snapshot = engine.refresh().unwrap();
        engine.close().unwrap();
        directory.acquired.store(0, Ordering::SeqCst);
        directory.lock_closes.store(0, Ordering::SeqCst);
        directory.mode.store(mode, Ordering::SeqCst);
        let result = Engine::open(&observed, schema.clone(), Some(checkpoint.clone()));
        let expected = if mode == 10 {
            Error::Close {
                cleanup: CleanupStatus::NativeFailure,
                release: ReleaseStatus::Failed,
            }
        } else {
            Error::Native
        };
        assert!(matches!(result, Err(error) if error == expected));
        assert_eq!(
            directory.acquired.load(Ordering::SeqCst),
            usize::from(mode != 7)
        );
        assert_eq!(
            directory.lock_closes.load(Ordering::SeqCst),
            usize::from(mode != 7)
        );
        assert_eq!(
            directory.failures.load(Ordering::SeqCst),
            if mode == 10 { 2 } else { 1 }
        );
        directory.mode.store(0, Ordering::SeqCst);
        let reopened = Engine::open(&observed, schema.clone(), Some(checkpoint));
        if mode == 7 {
            reopened.unwrap().close().unwrap();
        } else {
            assert!(matches!(reopened, Err(Error::Checkpoint)));
        }
        assert_eq!(snapshot.get("id", &[]).unwrap().unwrap().version, 1);
    }
    for mode in [0, 6] {
        let directory = FaultDirectory::new();
        let observed = crate::EngineDirectory::new(&directory);
        let schema = schema();
        let engine = Engine::open(&observed, schema.clone(), None).unwrap();
        let checkpoint = engine.commit().unwrap();
        engine.close().unwrap();
        let closes = directory.lock_closes.load(Ordering::SeqCst);
        directory.mode.store(mode, Ordering::SeqCst);
        let result =
            Engine::open_configured(&observed, schema.clone(), Some(checkpoint.clone()), |_| {
                Err(Error::Native)
            });
        let expected = if mode == 6 {
            Error::Close {
                cleanup: CleanupStatus::NativeFailure,
                release: ReleaseStatus::Failed,
            }
        } else {
            Error::Native
        };
        assert!(matches!(result, Err(error) if error == expected));
        assert_eq!(directory.lock_closes.load(Ordering::SeqCst), closes + 1);
        directory.mode.store(0, Ordering::SeqCst);
        assert!(matches!(
            Engine::open(&observed, schema, Some(checkpoint)),
            Err(Error::Checkpoint)
        ));
    }
}

#[test]
fn post_acquisition_configuration_unwind_cannot_certify_lineage() {
    let directory = FaultDirectory::new();
    let observed = crate::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ =
            Engine::open_configured(&observed, schema.clone(), Some(checkpoint.clone()), |_| {
                panic!("controlled post-acquisition configuration unwind");
            });
    }));
    assert!(result.is_err());
    assert_eq!(directory.lock_closes.load(Ordering::SeqCst), 2);
    assert!(matches!(
        Engine::open(&observed, schema, Some(checkpoint)),
        Err(Error::Poisoned)
    ));
}

#[test]
fn poisoned_native_owner_and_actual_release_failure_preserve_both_statuses() {
    let directory = FaultDirectory::new();
    let observed = crate::EngineDirectory::new(&directory);
    let schema = schema();
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    engine.apply(batch(&schema, 1)).unwrap();
    let checkpoint = engine.commit().unwrap();
    let snapshot = engine.refresh().unwrap();
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _writer = engine.writer.lock().unwrap();
        panic!("controlled native owner poison");
    }));
    assert!(unwind.is_err());
    directory.mode.store(6, Ordering::SeqCst);
    assert_eq!(
        engine.close(),
        Err(Error::Close {
            cleanup: CleanupStatus::Poisoned,
            release: ReleaseStatus::Failed,
        })
    );
    assert_eq!(directory.lock_closes.load(Ordering::SeqCst), 1);
    directory.mode.store(0, Ordering::SeqCst);
    assert!(matches!(
        Engine::open(&observed, schema, Some(checkpoint)),
        Err(Error::Checkpoint)
    ));
    assert_eq!(snapshot.get("id", &[]).unwrap().unwrap().version, 1);
}

fn text_schema(fields: usize) -> Schema {
    let names = ["t0", "t1", "t2"];
    let declared: Vec<_> = names[..fields]
        .iter()
        .map(|n| (*n, crate::Kind::Text))
        .collect();
    Schema::new(1, &declared).unwrap()
}

fn put_texts(schema: &Schema, texts: &[&str]) -> Result<PreparedBatch> {
    let names = ["t0", "t1", "t2"];
    let fields: Vec<_> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| (names[i], Value::String(t)))
        .collect();
    schema.prepare(
        "request",
        &[Mutation::Put {
            id: "id",
            version: 1,
            fields: &fields,
        }],
    )
}

#[test]
fn the_term_bound_counts_analyzed_tokens_not_bytes_across_all_text_fields() {
    let one = text_schema(1);
    // Short text: bytes alone prove it is within the bound.
    assert!(put_texts(&one, &["quick brown fox"]).is_ok());
    // Many bytes, few tokens: within the bound.
    assert!(put_texts(&one, &[&"word ".repeat(MAX_TERMS)]).is_ok());
    // One token over the bound, with long words and with one-byte words.
    assert!(matches!(
        put_texts(&one, &[&"word ".repeat(MAX_TERMS + 1)]),
        Err(Error::Limit)
    ));
    assert!(matches!(
        put_texts(&one, &[&"a ".repeat(MAX_TERMS + 1)]),
        Err(Error::Limit)
    ));
    // The bound is per document, across fields: each field alone is fine.
    let three = text_schema(3);
    let fifty = "a ".repeat(50);
    assert!(put_texts(&three, &[&fifty, &fifty, "x"]).is_ok());
    assert!(matches!(
        put_texts(&three, &[&fifty, &fifty, &fifty]),
        Err(Error::Limit)
    ));
}
