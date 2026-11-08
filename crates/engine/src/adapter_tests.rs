//! Private boundary controls use real native commits, then inject read failure.
use super::*;
use std::sync::atomic::Ordering;
#[path = "../tests/support/adapter_directory.rs"]
mod support;
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
    let schema = schema();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
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
        Engine::open(&directory, schema, Some(old_checkpoint)),
        Err(Error::Checkpoint)
    ));
}
#[test]
fn epoch_exhaustion_refuses_before_native_commit_or_checkpoint_invalidation() {
    let directory = FaultDirectory::new();
    let schema = schema();
    let engine = Engine::open(&directory, schema, None).unwrap();
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
    let schema = schema();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    let old = engine.commit().unwrap();
    let current = engine.commit().unwrap();
    engine.close().unwrap();
    // Deliberately alias the old descriptor to the current one. This isolates
    // epoch discrimination from whether the native writer advances on a no-op.
    let mut aliased = old;
    aliased.native = current.native;
    assert!(matches!(
        Engine::open(&directory, schema.clone(), Some(aliased)),
        Err(Error::Checkpoint)
    ));
    Engine::open(&directory, schema, Some(current))
        .unwrap()
        .close()
        .unwrap();
}
#[test]
fn poisoned_mutexes_and_redacted_errors_have_honest_continuation() {
    let directory = FaultDirectory::new();
    let schema = schema();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
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
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    let checkpoint = engine.commit().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _writer = engine.writer.lock().unwrap();
        panic!("injected writer unwind");
    }));
    assert!(result.is_err());
    assert_eq!(engine.close(), Err(Error::Poisoned));
    assert!(matches!(
        Engine::open(&directory, schema, Some(checkpoint)),
        Err(Error::Checkpoint)
    ));
    assert_eq!(Error::Native.to_string(), "engine Native");
    assert_eq!(Error::Poisoned.to_string(), "engine Poisoned");
}
#[test]
fn failed_reopen_io_has_no_writer_or_lineage_effects() {
    let directory = FaultDirectory::new();
    let schema = schema();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    for mode in [4, 5] {
        directory.mode.store(mode, Ordering::SeqCst);
        assert!(matches!(
            Engine::open(&directory, schema.clone(), Some(checkpoint.clone())),
            Err(Error::Native)
        ));
    }
    directory.mode.store(0, Ordering::SeqCst);
    Engine::open(&directory, schema, Some(checkpoint))
        .unwrap()
        .close()
        .unwrap();
}
