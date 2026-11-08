use lucene_store::{ByteBuffersDirectory, Directory, FsIndexOutput, Input, Lock};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, AtomicUsize, Ordering},
    mpsc::{Receiver, SyncSender},
};
use std::time::Duration;

pub(super) struct Block {
    pub(super) entered: SyncSender<()>,
    pub(super) resume: Receiver<()>,
}
pub(super) struct FaultDirectory {
    inner: ByteBuffersDirectory,
    pub(super) mode: Arc<AtomicU8>,
    pub(super) failures: Arc<AtomicUsize>,
    pub(super) lock_closes: Arc<AtomicUsize>,
    pub(super) block: Mutex<Option<Block>>,
}
impl FaultDirectory {
    pub(super) fn new() -> Self {
        Self {
            inner: ByteBuffersDirectory::new(),
            mode: Arc::new(AtomicU8::new(0)),
            failures: Arc::new(AtomicUsize::new(0)),
            lock_closes: Arc::new(AtomicUsize::new(0)),
            block: Mutex::new(None),
        }
    }
    fn fail(&self, mode: u8) -> lucene_store::Result<()> {
        if self.mode.load(Ordering::SeqCst) == mode {
            self.failures.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("injected native failure").into())
        } else {
            Ok(())
        }
    }
}
impl Directory for FaultDirectory {
    fn list_all(&self) -> lucene_store::Result<Vec<String>> {
        self.fail(5)?;
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
        Ok(Box::new(CountedLock {
            inner: self.inner.obtain_lock(name)?,
            mode: Arc::clone(&self.mode),
            failures: Arc::clone(&self.failures),
            closes: Arc::clone(&self.lock_closes),
        }))
    }
}

#[derive(Debug)]
struct CountedLock {
    inner: Box<dyn Lock>,
    mode: Arc<AtomicU8>,
    failures: Arc<AtomicUsize>,
    closes: Arc<AtomicUsize>,
}
impl Lock for CountedLock {
    fn close(&self) -> lucene_store::Result<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        if self.mode.load(Ordering::SeqCst) == 6 {
            self.failures.fetch_add(1, Ordering::SeqCst);
            return Err(std::io::Error::other("injected lock release failure").into());
        }
        self.inner.close()
    }
    fn ensure_valid(&self) -> lucene_store::Result<()> {
        self.inner.ensure_valid()
    }
}
