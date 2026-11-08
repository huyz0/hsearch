use lucene_store::{ByteBuffersDirectory, Directory, FsIndexOutput, Input, Lock};
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    mpsc::{Receiver, SyncSender},
};
use std::time::Duration;

#[derive(Debug)]
pub(super) struct Block {
    pub(super) entered: SyncSender<()>,
    pub(super) resume: Receiver<()>,
}
pub(super) struct FaultDirectory {
    inner: Arc<dyn Directory>,
    pub(super) mode: Arc<AtomicU8>,
    pub(super) failures: Arc<AtomicUsize>,
    pub(super) lock_closes: Arc<AtomicUsize>,
    pub(super) block: Mutex<Option<Block>>,
    pub(super) release_block: Arc<Mutex<Option<Block>>>,
    pub(super) acquired: Arc<AtomicUsize>,
    pub(super) release_started: Arc<AtomicBool>,
    pub(super) expect_drop: Arc<AtomicBool>,
    pub(super) writer_dropped: Arc<AtomicBool>,
    pub(super) early_release: Arc<AtomicUsize>,
    lock: Mutex<Weak<CountedLock>>,
}
impl FaultDirectory {
    pub(super) fn new() -> Self {
        Self::with_inner(Arc::new(ByteBuffersDirectory::new()))
    }
    pub(super) fn with_inner(inner: Arc<dyn Directory>) -> Self {
        Self {
            inner,
            mode: Arc::new(AtomicU8::new(0)),
            failures: Arc::new(AtomicUsize::new(0)),
            lock_closes: Arc::new(AtomicUsize::new(0)),
            block: Mutex::new(None),
            release_block: Arc::new(Mutex::new(None)),
            acquired: Arc::default(),
            release_started: Arc::default(),
            expect_drop: Arc::default(),
            writer_dropped: Arc::default(),
            early_release: Arc::default(),
            lock: Mutex::new(Weak::new()),
        }
    }
    // Controlled actual-lock early-release mutant. No synthetic acknowledgement.
    pub(super) fn release_early(&self) {
        self.lock
            .lock()
            .unwrap()
            .upgrade()
            .unwrap()
            .close()
            .unwrap();
    }
    fn fail(&self, mode: u8) -> lucene_store::Result<()> {
        let active = self.mode.load(Ordering::SeqCst);
        if active == mode || (mode == 5 && active == 9) || (mode == 8 && active == 10) {
            self.failures.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("injected native failure").into())
        } else {
            Ok(())
        }
    }
    fn before_output(&self) -> lucene_store::Result<()> {
        self.fail(1)?;
        let block = self.block.lock().unwrap().take();
        if let Some(block) = block {
            block
                .entered
                .send(())
                .map_err(|_| std::io::Error::other("gate observer closed"))?;
            block
                .resume
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| std::io::Error::other("gate release unavailable"))?;
        }
        Ok(())
    }
}
impl Directory for FaultDirectory {
    fn list_all(&self) -> lucene_store::Result<Vec<String>> {
        self.fail(5)?;
        if self.acquired.load(Ordering::SeqCst) > 0 {
            self.fail(8)?;
        }
        self.inner.list_all()
    }
    fn open(&self, name: &str) -> lucene_store::Result<Input> {
        self.fail(4)?;
        self.inner.open(name)
    }
    fn file_length(&self, name: &str) -> lucene_store::Result<u64> {
        self.inner.file_length(name)
    }
    fn create_output(&self, name: &str) -> lucene_store::Result<FsIndexOutput> {
        self.before_output()?;
        self.inner.create_output(name)
    }
    fn create_temp_output(
        &self,
        prefix: &str,
        suffix: &str,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.fail(1)?;
        self.inner.create_temp_output(prefix, suffix)
    }
    fn create_output_with_estimate(
        &self,
        name: &str,
        estimated_bytes: Option<u64>,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.before_output()?;
        self.inner
            .create_output_with_estimate(name, estimated_bytes)
    }
    fn create_temp_output_with_estimate(
        &self,
        prefix: &str,
        suffix: &str,
        estimated_bytes: Option<u64>,
    ) -> lucene_store::Result<FsIndexOutput> {
        self.fail(1)?;
        self.inner
            .create_temp_output_with_estimate(prefix, suffix, estimated_bytes)
    }
    fn sync(&self, names: &[String]) -> lucene_store::Result<()> {
        self.fail(2)?;
        self.inner.sync(names)
    }
    fn rename(&self, source: &str, dest: &str) -> lucene_store::Result<()> {
        self.fail(3)?;
        self.inner.rename(source, dest)
    }
    fn delete_file(&self, name: &str) -> lucene_store::Result<()> {
        self.inner.delete_file(name)
    }
    fn sync_meta_data(&self) -> lucene_store::Result<()> {
        self.fail(2)?;
        self.inner.sync_meta_data()
    }
    fn obtain_lock(&self, name: &str) -> lucene_store::Result<Box<dyn Lock>> {
        self.fail(7)?;
        let inner = self.inner.obtain_lock(name)?;
        self.acquired.fetch_add(1, Ordering::SeqCst);
        let lock = Arc::new(CountedLock {
            inner,
            mode: Arc::clone(&self.mode),
            failures: Arc::clone(&self.failures),
            closes: Arc::clone(&self.lock_closes),
            release_block: Arc::clone(&self.release_block),
            release_started: Arc::clone(&self.release_started),
            expect_drop: Arc::clone(&self.expect_drop),
            writer_dropped: Arc::clone(&self.writer_dropped),
            early_release: Arc::clone(&self.early_release),
        });
        *self.lock.lock().unwrap() = Arc::downgrade(&lock);
        Ok(Box::new(SharedCountedLock(lock)))
    }
    fn pending_deletions(&self) -> lucene_store::Result<std::collections::BTreeSet<String>> {
        self.inner.pending_deletions()
    }
    fn fs_directory_path(&self) -> Option<&std::path::Path> {
        self.inner.fs_directory_path()
    }
    fn copy_from(&self, from: &dyn Directory, src: &str, dest: &str) -> lucene_store::Result<()> {
        self.inner.copy_from(from, src, dest)
    }
}

#[derive(Debug)]
struct CountedLock {
    inner: Box<dyn Lock>,
    mode: Arc<AtomicU8>,
    failures: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
    release_block: Arc<Mutex<Option<Block>>>,
    release_started: Arc<AtomicBool>,
    expect_drop: Arc<AtomicBool>,
    writer_dropped: Arc<AtomicBool>,
    early_release: Arc<AtomicUsize>,
}
impl Lock for CountedLock {
    fn close(&self) -> lucene_store::Result<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        if self.mode.load(Ordering::SeqCst) == 11 {
            panic!("controlled actual release panic");
        }
        self.release_started.store(true, Ordering::SeqCst);
        if self.expect_drop.load(Ordering::SeqCst) && !self.writer_dropped.load(Ordering::SeqCst) {
            self.early_release.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(block) = self.release_block.lock().unwrap().take() {
            block
                .entered
                .send(())
                .map_err(|_| std::io::Error::other("release observer closed"))?;
            block
                .resume
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| std::io::Error::other("release observer timed out"))?;
        }
        if matches!(self.mode.load(Ordering::SeqCst), 6 | 9 | 10) {
            self.failures.fetch_add(1, Ordering::SeqCst);
            return Err(std::io::Error::other("injected lock release failure").into());
        }
        self.inner.close()
    }
    fn ensure_valid(&self) -> lucene_store::Result<()> {
        self.inner.ensure_valid()
    }
}

#[derive(Debug)]
struct SharedCountedLock(Arc<CountedLock>);
impl Lock for SharedCountedLock {
    fn close(&self) -> lucene_store::Result<()> {
        self.0.close()
    }
    fn ensure_valid(&self) -> lucene_store::Result<()> {
        self.0.ensure_valid()
    }
}

// Neither counted wrapper has a Drop that calls close. The underlying real
// backend retains its own destructor behavior, which cannot generate a count.
