//! Merges on a background thread: a refresh never merges, the thread keeps the
//! segments few, a snapshot opened before a merge still answers, close joins
//! the thread, and a panic in native code poisons the engine and its close.
use super::*;
use crate::adapter_test_support::FaultDirectory;
use crate::{Kind, Limits, Query};

fn schema() -> Schema {
    Schema::new(1, &[("body", Kind::Text)])
        .unwrap()
        .with_limits(Limits::scale().with_document_bytes(1 << 20))
        .unwrap()
        .with_auto_merge_mb(16)
}

fn put(schema: &Schema, i: usize) -> PreparedBatch {
    schema
        .prepare(
            &format!("r{i}"),
            &[Mutation::Put {
                id: &format!("d{i}"),
                version: 1,
                fields: &[("body", Value::String("quick fox"))],
            }],
        )
        .unwrap()
}

fn found(snapshot: &crate::reader::Snapshot) -> u64 {
    let fox = Query::Term {
        field: "body",
        value: Value::String("fox"),
    };
    snapshot.search(&fox, &[], 0, &[]).unwrap().total
}

#[test]
fn a_refresh_never_merges_and_the_thread_does() {
    let schema = schema();
    let directory = FaultDirectory::new();
    let observed = crate::EngineDirectory::new(&directory);
    let engine =
        Engine::open_merging(&observed, schema.clone(), None, MergeMode::Background).unwrap();
    let mut first = None;
    let mut last = None;
    for i in 0..40 {
        engine.apply(put(&schema, i)).unwrap();
        let snapshot = engine.refresh().unwrap();
        if i == 0 {
            first = Some(snapshot);
        } else {
            last = Some(snapshot);
        }
    }
    // No merge thread yet: every refresh added a segment and none merged.
    assert_eq!(last.as_ref().unwrap().reader.segment_readers().len(), 40);
    let first = first.unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let merger = scope.spawn(|| engine.run_background_merges(&stop));
        let mut segments = 40;
        for _ in 0..200 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            segments = engine.refresh().unwrap().reader.segment_readers().len();
            if segments <= 20 {
                break;
            }
        }
        stop.store(true, Ordering::Release);
        assert!(merger.join().unwrap() > 0, "no merge ran");
        assert!(segments <= 20, "{segments} segments");
    });
    // Postings survive, and the snapshot taken before any merge still answers.
    assert_eq!(found(&engine.refresh().unwrap()), 40);
    assert_eq!(found(&first), 1);
    assert!(first.get("d0", &["body"]).unwrap().is_some());
    engine.close().unwrap();
}

#[test]
fn an_inline_engine_has_no_background_work_and_zero_merge_size_stays_inline() {
    let directory = FaultDirectory::new();
    let observed = crate::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed, schema(), None).unwrap();
    assert_eq!(engine.run_background_merges(&AtomicBool::new(true)), 0);
    engine.close().unwrap();
    let off = schema().with_auto_merge_mb(0);
    let engine = Engine::open_merging(&observed, off, None, MergeMode::Background).unwrap();
    assert!(matches!(engine.writer, Writer::Inline(_)));
    engine.close().unwrap();
}

#[test]
fn a_native_panic_poisons_a_background_engine_and_its_close() {
    let schema = schema();
    let directory = FaultDirectory::new();
    let observed = crate::EngineDirectory::new(&directory);
    let engine =
        Engine::open_merging(&observed, schema.clone(), None, MergeMode::Background).unwrap();
    engine.apply(put(&schema, 0)).unwrap();
    let Writer::Background(_, panicked) = &engine.writer else {
        panic!("not background");
    };
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        guarded(panicked, || panic!("injected native panic"))
    }));
    assert!(unwind.is_err());
    assert_eq!(engine.apply(put(&schema, 1)), Err(Error::Poisoned));
    assert_eq!(engine.commit().map(|_| ()), Err(Error::Poisoned));
    assert_eq!(engine.close(), Err(Error::Poisoned));
}

#[test]
fn local_engine_runs_and_joins_its_merge_thread() {
    let root = std::env::temp_dir().join(format!("hsearch-bgmerge-{}", std::process::id()));
    let schema = schema();
    let total = crate::with_local_engine_merging(
        &root,
        schema.clone(),
        MergeMode::Background,
        |engine, _| {
            for i in 0..30 {
                engine.apply(put(&schema, i)).unwrap();
                drop(engine.refresh().unwrap());
            }
            found(&engine.refresh().unwrap())
        },
    )
    .unwrap();
    assert_eq!(total, 30);
    let _ = std::fs::remove_dir_all(&root);
}
