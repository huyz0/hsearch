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
    // Boundary of the byte shortcut: exactly 128 and 129 bytes, both within the token bound.
    assert!(put_texts(&one, &[&"a".repeat(MAX_TERMS)]).is_ok());
    assert!(put_texts(&one, &[&"a".repeat(MAX_TERMS + 1)]).is_ok());
    assert!(put_texts(&one, &[&"a ".repeat(MAX_TERMS / 2)]).is_ok());
    // The bound is per document, across fields: each field alone is fine.
    let three = text_schema(3);
    let fifty = "a ".repeat(50);
    assert!(put_texts(&three, &[&fifty, &fifty, "x"]).is_ok());
    assert!(matches!(
        put_texts(&three, &[&fifty, &fifty, &fifty]),
        Err(Error::Limit)
    ));
}

#[test]
fn live_identities_keep_a_digest_and_never_the_text() {
    use crate::{Kind, Limits};
    let schema = Schema::new(1, &[("body", Kind::Text)])
        .unwrap()
        .with_limits(Limits::scale().with_document_bytes(1 << 20))
        .unwrap();
    let directory = FaultDirectory::new();
    let observed = crate::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    let apply = |version: i64, text: &str| {
        let batch = schema
            .prepare(
                &format!("r{version}"),
                &[Mutation::Put {
                    id: "doc",
                    version,
                    fields: &[("body", Value::String(text))],
                }],
            )
            .unwrap();
        engine.apply(batch).unwrap();
    };
    apply(1, &"alpha ".repeat(100_000));
    let first = engine.state.lock().unwrap().current["doc"];
    apply(2, &"beta ".repeat(100_000));
    let second = engine.state.lock().unwrap().current["doc"];
    assert!(first.digest.is_some() && first.digest != second.digest);
    assert_eq!((first.version, second.version), (1, 2));
    // A checkpoint taken now reopens: the digest equals the stored values'.
    let checkpoint = engine.commit().unwrap();
    engine.close().unwrap();
    assert!(Engine::open(&observed, schema, Some(checkpoint)).is_ok());
}

#[test]
fn retry_history_holds_a_digest_and_still_tells_a_retry_from_a_conflict() {
    use crate::{Kind, Limits};
    let schema = Schema::new(1, &[("body", Kind::Text)])
        .unwrap()
        .with_limits(Limits::scale().with_document_bytes(1 << 20))
        .unwrap();
    let directory = FaultDirectory::new();
    let observed = crate::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    let put = |request: &str, text: &str| {
        schema
            .prepare(
                request,
                &[Mutation::Put {
                    id: "doc",
                    version: 1,
                    fields: &[("body", Value::String(text))],
                }],
            )
            .unwrap()
    };
    let text = "alpha ".repeat(100_000);
    let receipts = engine.apply(put("r1", &text)).unwrap();
    // The same request with the same bytes is a retry; the receipts are replayed.
    assert_eq!(engine.apply(put("r1", &text)).unwrap(), receipts);
    // One changed final byte under the same request id is a conflict.
    let mut changed = text.clone();
    changed.pop();
    changed.push('b');
    assert!(matches!(
        engine.apply(put("r1", &changed)),
        Err(Error::Conflict)
    ));
    let state = engine.state.lock().unwrap();
    assert!(state.history[0].operations[0].digest.is_some());
}

#[test]
fn a_retry_differing_in_kind_version_value_type_or_delete_is_a_conflict() {
    use crate::Kind;
    let schema = Schema::new(
        1,
        &[
            ("body", Kind::Text),
            ("n", Kind::Integer),
            ("ok", Kind::Boolean),
        ],
    )
    .unwrap();
    let directory = FaultDirectory::new();
    let observed = crate::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    let batch = |m: Mutation<'_>| schema.prepare("r", &[m]).unwrap();
    let fields = |n: i64, ok: bool| {
        [
            ("body", Value::String("x")),
            ("n", Value::Integer(n)),
            ("ok", Value::Boolean(ok)),
        ]
    };
    let base = fields(1, true);
    let put = Mutation::Put {
        id: "d",
        version: 1,
        fields: &base,
    };
    let receipts = engine.apply(batch(put)).unwrap();
    assert_eq!(engine.apply(batch(put)).unwrap(), receipts);
    let other_int = fields(2, true);
    let other_bool = fields(1, false);
    for different in [
        Mutation::Put {
            id: "d",
            version: 1,
            fields: &other_int,
        },
        Mutation::Put {
            id: "d",
            version: 1,
            fields: &other_bool,
        },
        Mutation::Put {
            id: "d",
            version: 2,
            fields: &base,
        },
        Mutation::Create {
            id: "d",
            version: 1,
            fields: &base,
        },
        Mutation::Delete {
            id: "d",
            version: 1,
        },
    ] {
        assert!(
            matches!(engine.apply(batch(different)), Err(Error::Conflict)),
            "{different:?}"
        );
    }
}

/// One document per refresh, `cycles` times; the segment count of the last snapshot,
/// the first snapshot kept open throughout, and the document count of the last.
fn refresh_cycles(auto_merge_mb: usize, cycles: usize) -> (usize, crate::reader::Snapshot, u64) {
    use crate::{Kind, Limits, Query};
    let schema = Schema::new(1, &[("body", Kind::Text)])
        .unwrap()
        .with_limits(Limits::scale().with_document_bytes(1 << 20))
        .unwrap()
        .with_auto_merge_mb(auto_merge_mb);
    let directory = FaultDirectory::new();
    let observed = crate::EngineDirectory::new(&directory);
    let engine = Engine::open(&observed, schema.clone(), None).unwrap();
    let mut first = None;
    let mut last = None;
    for i in 0..cycles {
        let id = format!("d{i}");
        let batch = schema
            .prepare(
                &format!("r{i}"),
                &[Mutation::Put {
                    id: &id,
                    version: 1,
                    fields: &[("body", Value::String("quick fox"))],
                }],
            )
            .unwrap();
        engine.apply(batch).unwrap();
        let snapshot = engine.refresh().unwrap();
        if i == 0 {
            first = Some(snapshot);
        } else {
            last = Some(snapshot);
        }
    }
    let last = last.unwrap();
    let segments = last.reader.segment_readers().len();
    let total = last.search(&Query::All, &[], 0, &[]).unwrap().total;
    drop(engine);
    (segments, first.unwrap(), total)
}

#[test]
fn without_auto_merge_every_refresh_adds_a_segment() {
    let (segments, _, total) = refresh_cycles(0, 40);
    assert_eq!((segments, total), (40, 40));
}

#[test]
fn auto_merge_keeps_segments_few_and_an_open_snapshot_readable() {
    let (segments, first, total) = refresh_cycles(16, 40);
    assert_eq!(total, 40);
    assert!(segments <= 20, "{segments} segments after 40 refreshes");
    // A snapshot taken before the merges still answers from its own segments.
    assert_eq!(
        first.search(&crate::Query::All, &[], 0, &[]).unwrap().total,
        1
    );
    assert!(first.get("d0", &["body"]).unwrap().is_some());
}
