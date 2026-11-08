//! Real acquired locks and native writer owners; no fabricated release receipt.
use super::*;
use crate::{CleanupStatus, Engine, Kind, Mutation, Query, Schema, Value};
use lucene_index::deletion_policy::{IndexCommit, IndexDeletionPolicy};
use lucene_index::index_writer::IndexWriter;
use lucene_index::segment_info::LuceneVersion;
use lucene_store::{ByteBuffersDirectory, FsDirectory};
use std::sync::{atomic::AtomicUsize, mpsc};
use std::time::Duration;

#[path = "../tests/support/adapter_directory.rs"]
mod support;
use support::{Block, FaultDirectory};

fn schema() -> Schema {
    Schema::new(1, &[("key", Kind::Keyword)]).unwrap()
}

// Unique, bounded test-only filesystem ownership. No upstream test features.
fn with_backend(filesystem: bool, run: impl FnOnce(&FaultDirectory)) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    if !filesystem {
        run(&FaultDirectory::new());
        return;
    }
    let parent = std::env::temp_dir().canonicalize().unwrap();
    let path = parent.join(format!(
        "hsearch-release-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir(&path).unwrap();
    let canonical = path.canonicalize().unwrap();
    assert_eq!(canonical.parent(), Some(parent.as_path()));
    let directory = FaultDirectory::with_inner(Arc::new(FsDirectory::open(canonical.clone())));
    run(&directory);
    drop(directory);
    let entries = std::fs::read_dir(&canonical)
        .unwrap()
        .take(257)
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    assert!(entries.len() <= 256);
    for entry in entries {
        assert!(entry.file_type().unwrap().is_file());
        std::fs::remove_file(entry.path()).unwrap();
    }
    std::fs::remove_dir(canonical).unwrap();
}

#[test]
fn native_cleanup_and_actual_release_have_distinct_results_on_both_backends() {
    for filesystem in [false, true] {
        for mode in [0, 5, 6, 9] {
            with_backend(filesystem, |raw| {
                let directory = EngineDirectory::new(raw);
                assert!(std::ptr::eq(*directory.inner(), raw));
                let schema = schema();
                let engine = Engine::open(&directory, schema.clone(), None).unwrap();
                engine
                    .apply(
                        schema
                            .prepare(
                                "one",
                                &[Mutation::Put {
                                    id: "id",
                                    version: 1,
                                    fields: &[("key", Value::String("old"))],
                                }],
                            )
                            .unwrap(),
                    )
                    .unwrap();
                let checkpoint = engine.commit().unwrap();
                let snapshot = engine.refresh().unwrap();
                raw.mode.store(mode, Ordering::SeqCst);
                let result = engine.close();
                assert_eq!(raw.lock_closes.load(Ordering::SeqCst), 1);
                assert_eq!(
                    raw.failures.load(Ordering::SeqCst),
                    match mode {
                        0 => 0,
                        9 => 2,
                        _ => 1,
                    }
                );
                assert_eq!(
                    result,
                    match mode {
                        0 => Ok(()),
                        5 => Err(Error::Native),
                        6 => Err(Error::Close {
                            cleanup: CleanupStatus::Succeeded,
                            release: ReleaseStatus::Failed
                        }),
                        9 => Err(Error::Close {
                            cleanup: CleanupStatus::NativeFailure,
                            release: ReleaseStatus::Failed
                        }),
                        _ => unreachable!(),
                    }
                );
                raw.mode.store(0, Ordering::SeqCst);
                assert_eq!(
                    snapshot.search(&Query::All, &[], 128, &[]).unwrap().total,
                    1
                );
                let reopened = Engine::open(&directory, schema, Some(checkpoint));
                if mode == 0 {
                    reopened.unwrap().close().unwrap();
                } else {
                    assert!(matches!(reopened, Err(Error::Checkpoint)));
                }
            });
        }
    }
}

#[test]
fn raw_backend_stays_locked_until_actual_release_and_close_has_not_returned() {
    for filesystem in [false, true] {
        with_backend(filesystem, |raw| {
            let directory = EngineDirectory::new(raw);
            let engine = Engine::open(&directory, schema(), None).unwrap();
            let (entered_tx, entered_rx) = mpsc::sync_channel(1);
            let (resume_tx, resume_rx) = mpsc::sync_channel(1);
            *raw.release_block.lock().unwrap() = Some(Block {
                entered: entered_tx,
                resume: resume_rx,
            });
            let observation = directory
                .observed
                .slot
                .lock()
                .unwrap()
                .as_ref()
                .cloned()
                .unwrap();
            std::thread::scope(|scope| {
                let closing = scope.spawn(move || engine.close());
                entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let contender = raw.obtain_lock("write.lock");
                let premature = closing.is_finished();
                let slot_unlocked = directory.observed.slot.try_lock().is_ok();
                let observation_unlocked = observation.phase.try_lock().is_ok();
                resume_tx.send(()).unwrap();
                closing.join().unwrap().unwrap();
                assert!(
                    contender.is_err(),
                    "must test real backend, not observer Busy"
                );
                assert!(!premature);
                assert!(
                    slot_unlocked && observation_unlocked,
                    "no observer mutex across backend close"
                );
            });
            let next = raw.obtain_lock("write.lock").unwrap();
            next.close().unwrap();
        });
    }
}

#[test]
fn missing_foreign_duplicate_stale_and_poisoned_observation_never_acknowledge() {
    for corruption in 0..5 {
        let raw = FaultDirectory::new();
        let directory = EngineDirectory::new(&raw);
        let attempt = directory.begin().unwrap();
        let lock = directory.observed.obtain_lock("write.lock").unwrap();
        match corruption {
            0 => {
                *directory.observed.slot.lock().unwrap() = None;
            }
            1 => {
                *directory.observed.slot.lock().unwrap() = Some(Arc::new(Observation::new()));
            }
            2 => {
                assert!(directory.observed.obtain_lock("write.lock").is_err());
            }
            3 => {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _phase = attempt.observation.phase.lock().unwrap();
                    panic!("controlled observation poison");
                }));
            }
            4 => {
                attempt.observation.invalid.store(true, Ordering::Release);
            }
            _ => unreachable!(),
        }
        drop(lock);
        assert_eq!(raw.lock_closes.load(Ordering::SeqCst), 1);
        assert_eq!(attempt.status(), ReleaseStatus::Unobserved);
        assert!(!attempt.acquired());
    }
    let raw = FaultDirectory::new();
    let directory = EngineDirectory::new(&raw);
    let old = directory.begin().unwrap();
    drop(directory.observed.obtain_lock("write.lock").unwrap());
    let old_observation = Arc::clone(&old.observation);
    assert_eq!(old.status(), ReleaseStatus::Acknowledged);
    drop(old);
    let next = directory.begin().unwrap();
    assert!(!Arc::ptr_eq(&old_observation, &next.observation));
    assert_eq!(next.status(), ReleaseStatus::Unobserved);
    drop(directory.observed.obtain_lock("write.lock").unwrap());
    assert_eq!(next.status(), ReleaseStatus::Acknowledged);
    assert_eq!(raw.lock_closes.load(Ordering::SeqCst), 2);
}

#[test]
fn explicit_and_drop_release_delegate_once_and_cannot_promote_failure() {
    for mode in [0, 6] {
        let raw = FaultDirectory::new();
        let directory = EngineDirectory::new(&raw);
        let attempt = directory.begin().unwrap();
        let lock = directory.observed.obtain_lock("write.lock").unwrap();
        raw.mode.store(mode, Ordering::SeqCst);
        lock.ensure_valid().unwrap();
        assert_eq!(format!("{lock:?}"), "ObservedLock");
        assert_eq!(lock.close().is_ok(), mode == 0);
        raw.mode.store(0, Ordering::SeqCst);
        assert_eq!(lock.close().is_ok(), mode == 0);
        assert!(lock.ensure_valid().is_err());
        drop(lock);
        assert_eq!(raw.lock_closes.load(Ordering::SeqCst), 1);
        assert_eq!(
            attempt.status(),
            if mode == 0 {
                ReleaseStatus::Acknowledged
            } else {
                ReleaseStatus::Failed
            }
        );
    }
}

// Native policy objects must be 'static; retain only the exact owned backend
// and bounded counters in the real writer-owned policy below.
#[derive(Debug)]
struct Policy {
    dropped: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    after_release: Arc<AtomicUsize>,
    block: Mutex<Option<Block>>,
}
impl IndexDeletionPolicy for Policy {
    fn on_init(&self, _: &mut [IndexCommit]) -> lucene_index::deletion_policy::Result<()> {
        Ok(())
    }
    fn on_commit(&self, _: &mut [IndexCommit]) -> lucene_index::deletion_policy::Result<()> {
        Ok(())
    }
}
impl Drop for Policy {
    fn drop(&mut self) {
        if let Some(block) = self.block.lock().unwrap().take() {
            block.entered.send(()).unwrap();
            block.resume.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        if self.released.load(Ordering::SeqCst) {
            self.after_release.fetch_add(1, Ordering::SeqCst);
        }
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[test]
fn actual_writer_owned_policy_drops_before_lock_and_early_release_is_detected() {
    for filesystem in [false, true] {
        for early in [false, true] {
            with_backend(filesystem, |raw| {
                let directory = EngineDirectory::new(raw);
                let attempt = directory.begin().unwrap();
                let mut writer = IndexWriter::open(
                    &directory.observed,
                    vec![],
                    "Lucene104",
                    LuceneVersion {
                        major: 10,
                        minor: 5,
                        bugfix: 0,
                    },
                )
                .unwrap();
                writer.set_commit_on_close(false);
                raw.expect_drop.store(true, Ordering::SeqCst);
                let after_release = Arc::new(AtomicUsize::new(0));
                let (entered_tx, entered_rx) = mpsc::sync_channel(1);
                let (resume_tx, resume_rx) = mpsc::sync_channel(1);
                writer
                    .set_index_deletion_policy(Arc::new(Policy {
                        dropped: Arc::clone(&raw.writer_dropped),
                        released: Arc::clone(&raw.release_started),
                        after_release: Arc::clone(&after_release),
                        block: Mutex::new(Some(Block {
                            entered: entered_tx,
                            resume: resume_rx,
                        })),
                    }))
                    .unwrap();
                if early {
                    raw.release_early();
                }
                std::thread::scope(|scope| {
                    let closing = scope.spawn(move || writer.close());
                    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    let calls_before_owner_drop = raw.lock_closes.load(Ordering::SeqCst);
                    let competitor = raw.obtain_lock("write.lock");
                    resume_tx.send(()).unwrap();
                    closing.join().unwrap().unwrap();
                    assert_eq!(calls_before_owner_drop, usize::from(early));
                    assert_eq!(competitor.is_ok(), early);
                    if let Ok(lock) = competitor {
                        lock.close().unwrap();
                    }
                });
                assert!(raw.writer_dropped.load(Ordering::SeqCst));
                assert_eq!(raw.early_release.load(Ordering::SeqCst), usize::from(early));
                assert_eq!(after_release.load(Ordering::SeqCst), usize::from(early));
                assert_eq!(attempt.status(), ReleaseStatus::Acknowledged);
            });
        }
    }
}

#[test]
fn consuming_engine_close_rejects_missing_foreign_duplicate_and_poisoned_ack() {
    for corruption in 0..4 {
        let raw = FaultDirectory::new();
        let directory = EngineDirectory::new(&raw);
        let schema = schema();
        let engine = Engine::open(&directory, schema.clone(), None).unwrap();
        let checkpoint = engine.commit().unwrap();
        let original = directory
            .observed
            .slot
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .unwrap();
        match corruption {
            0 => {
                *directory.observed.slot.lock().unwrap() = None;
            }
            1 => {
                *directory.observed.slot.lock().unwrap() = Some(Arc::new(Observation::new()));
            }
            2 => {
                assert!(directory.observed.obtain_lock("write.lock").is_err());
            }
            3 => {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _guard = original.phase.lock().unwrap();
                    panic!("controlled close observation poison");
                }));
            }
            _ => unreachable!(),
        }
        assert_eq!(
            engine.close(),
            Err(Error::Close {
                cleanup: CleanupStatus::Succeeded,
                release: ReleaseStatus::Unobserved
            })
        );
        assert_eq!(raw.lock_closes.load(Ordering::SeqCst), 1);
        // A fresh wrapper isolates lineage refusal from a damaged observer slot.
        let replacement = EngineDirectory::new(&raw);
        assert!(matches!(
            Engine::open(&replacement, schema, Some(checkpoint)),
            Err(Error::Checkpoint)
        ));
    }
}

#[test]
fn acknowledged_implicit_drop_remains_uncertain_on_both_backends() {
    for filesystem in [false, true] {
        with_backend(filesystem, |raw| {
            let directory = EngineDirectory::new(raw);
            let schema = schema();
            let engine = Engine::open(&directory, schema.clone(), None).unwrap();
            let checkpoint = engine.commit().unwrap();
            drop(engine);
            assert_eq!(raw.lock_closes.load(Ordering::SeqCst), 1);
            assert!(matches!(
                Engine::open(&directory, schema, Some(checkpoint)),
                Err(Error::Checkpoint)
            ));
        });
    }
}

#[test]
fn missing_or_poisoned_slot_refuses_before_backend_acquisition() {
    let raw = FaultDirectory::new();
    let directory = EngineDirectory::new(&raw);
    {
        let _held = directory.observed.slot.lock().unwrap();
        assert!(matches!(
            Engine::open(&directory, schema(), None),
            Err(Error::Busy)
        ));
    }
    assert!(directory.observed.obtain_lock("write.lock").is_err());
    assert_eq!(raw.acquired.load(Ordering::SeqCst), 0);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = directory.observed.slot.lock().unwrap();
        panic!("controlled directory slot poison");
    }));
    assert!(matches!(
        Engine::open(&directory, schema(), None),
        Err(Error::Poisoned)
    ));
    assert_eq!(raw.acquired.load(Ordering::SeqCst), 0);
}

#[test]
fn actual_release_panic_leaves_lineage_uncertain_and_never_retries_explicitly() {
    let raw = FaultDirectory::new();
    let directory = EngineDirectory::new(&raw);
    let schema = schema();
    let engine = Engine::open(&directory, schema.clone(), None).unwrap();
    let checkpoint = engine.commit().unwrap();
    raw.mode.store(11, Ordering::SeqCst);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = engine.close();
    }));
    assert!(result.is_err());
    assert_eq!(raw.lock_closes.load(Ordering::SeqCst), 1);
    raw.mode.store(0, Ordering::SeqCst);
    assert!(matches!(
        Engine::open(&directory, schema, Some(checkpoint)),
        Err(Error::Checkpoint)
    ));
}

#[test]
fn prepared_native_commit_keeps_consuming_cleanup_and_release_order() {
    for filesystem in [false, true] {
        for commit_on_close in [false, true] {
            with_backend(filesystem, |raw| {
                let directory = EngineDirectory::new(raw);
                let attempt = directory.begin().unwrap();
                let mut writer = IndexWriter::open(
                    &directory.observed,
                    vec![],
                    "Lucene104",
                    LuceneVersion {
                        major: 10,
                        minor: 5,
                        bugfix: 0,
                    },
                )
                .unwrap();
                writer.set_commit_on_close(commit_on_close);
                writer.prepare_commit().unwrap();
                assert!(
                    raw.list_all()
                        .unwrap()
                        .iter()
                        .any(|name| name.starts_with("pending_segments_"))
                );
                assert_eq!(raw.lock_closes.load(Ordering::SeqCst), 0);
                writer.close().unwrap();
                assert_eq!(raw.lock_closes.load(Ordering::SeqCst), 1);
                assert_eq!(attempt.status(), ReleaseStatus::Acknowledged);
                assert!(
                    !raw.list_all()
                        .unwrap()
                        .iter()
                        .any(|name| name.starts_with("pending_segments_"))
                );
            });
        }
    }
}

struct Delegation {
    inner: ByteBuffersDirectory,
    seen: AtomicUsize,
}
impl Delegation {
    fn mark(&self, bit: usize) {
        self.seen.fetch_or(1 << bit, Ordering::SeqCst);
    }
}
impl Directory for Delegation {
    fn list_all(&self) -> lucene_store::Result<Vec<String>> {
        self.mark(0);
        self.inner.list_all()
    }
    fn open(&self, name: &str) -> lucene_store::Result<Input> {
        self.mark(1);
        self.inner.open(name)
    }
    fn file_length(&self, name: &str) -> lucene_store::Result<u64> {
        self.mark(2);
        self.inner.file_length(name)
    }
    fn create_output(&self, name: &str) -> lucene_store::Result<FsIndexOutput> {
        self.mark(3);
        self.inner.create_output(name)
    }
    fn create_output_with_estimate(
        &self,
        name: &str,
        size: Option<u64>,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.mark(4);
        self.inner.create_output_with_estimate(name, size)
    }
    fn create_temp_output(
        &self,
        prefix: &str,
        suffix: &str,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.mark(5);
        self.inner.create_temp_output(prefix, suffix)
    }
    fn create_temp_output_with_estimate(
        &self,
        prefix: &str,
        suffix: &str,
        size: Option<u64>,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.mark(6);
        self.inner
            .create_temp_output_with_estimate(prefix, suffix, size)
    }
    fn sync(&self, names: &[String]) -> lucene_store::Result<()> {
        self.mark(7);
        self.inner.sync(names)
    }
    fn rename(&self, from: &str, to: &str) -> lucene_store::Result<()> {
        self.mark(8);
        self.inner.rename(from, to)
    }
    fn delete_file(&self, name: &str) -> lucene_store::Result<()> {
        self.mark(9);
        self.inner.delete_file(name)
    }
    fn sync_meta_data(&self) -> lucene_store::Result<()> {
        self.mark(10);
        self.inner.sync_meta_data()
    }
    fn pending_deletions(&self) -> lucene_store::Result<BTreeSet<String>> {
        self.mark(11);
        Ok(BTreeSet::from(["pending.probe".into()]))
    }
    fn fs_directory_path(&self) -> Option<&Path> {
        self.mark(12);
        Some(Path::new("original-backend.probe"))
    }
    fn copy_from(&self, from: &dyn Directory, src: &str, dest: &str) -> lucene_store::Result<()> {
        self.mark(13);
        self.inner.copy_from(from, src, dest)
    }
    fn obtain_lock(&self, name: &str) -> lucene_store::Result<Box<dyn Lock>> {
        self.mark(14);
        self.inner.obtain_lock(name)
    }
}

#[test]
fn every_production_directory_method_reaches_the_same_backend() {
    let raw = Delegation {
        inner: ByteBuffersDirectory::new(),
        seen: AtomicUsize::new(0),
    };
    let owner = EngineDirectory::new(&raw);
    let directory = &owner.observed;
    directory.create_output("a").unwrap().close().unwrap();
    directory
        .create_output_with_estimate("estimated", Some(17))
        .unwrap()
        .close()
        .unwrap();
    directory
        .create_temp_output("p", "s")
        .unwrap()
        .close()
        .unwrap();
    directory
        .create_temp_output_with_estimate("p", "s", Some(19))
        .unwrap()
        .close()
        .unwrap();
    assert_eq!(directory.open("a").unwrap().len(), 0);
    assert_eq!(directory.file_length("a").unwrap(), 0);
    directory.sync(&["a".to_owned()]).unwrap();
    directory.rename("a", "b").unwrap();
    directory.sync_meta_data().unwrap();
    directory.copy_from(directory, "b", "c").unwrap();
    directory.delete_file("c").unwrap();
    assert!(directory.list_all().unwrap().iter().any(|name| name == "b"));
    assert_eq!(
        directory.pending_deletions().unwrap(),
        BTreeSet::from(["pending.probe".into()])
    );
    assert_eq!(
        directory.fs_directory_path(),
        Some(Path::new("original-backend.probe"))
    );
    directory
        .obtain_lock("other.lock")
        .unwrap()
        .close()
        .unwrap();
    assert_eq!(raw.seen.load(Ordering::SeqCst), (1 << 15) - 1);
}
