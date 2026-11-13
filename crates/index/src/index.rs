//! The handle callers hold: commands go to the writer thread, searches read the
//! latest published snapshot directly.
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use hsearch_engine::{Schema, Snapshot};
use hsearch_object::MetadataStore;

use crate::store::{StoreFiles, valid_name};
use crate::writer::{
    Bulk, Command, Hooks, ItemResult, NoHooks, Published, Shared, Wait, create_root, read_root, run,
};
use crate::{Error, Op, Result};

/// An open index with its single writer.
pub struct Index {
    commands: Sender<Command>,
    shared: Shared,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Index {
    /// Create a new index and open it. `scratch` is a disposable local
    /// directory; anything in it is deleted.
    pub fn create<S: MetadataStore + 'static>(
        store: Arc<S>,
        name: &str,
        schema: Schema,
        scratch: &Path,
    ) -> Result<Self> {
        if !valid_name(name) {
            return Err(Error::Invalid);
        }
        create_root(&*store, name)?;
        Self::open(store, name, schema, scratch)
    }

    /// Open an existing index by replaying its log from the store.
    pub fn open<S: MetadataStore + 'static>(
        store: Arc<S>,
        name: &str,
        schema: Schema,
        scratch: &Path,
    ) -> Result<Self> {
        Self::open_with(store, name, schema, scratch, Arc::new(NoHooks))
    }

    /// [`Index::open`] with observers at the crash points, for fault tests.
    pub fn open_with<S: MetadataStore + 'static>(
        store: Arc<S>,
        name: &str,
        schema: Schema,
        scratch: &Path,
        hooks: Arc<dyn Hooks>,
    ) -> Result<Self> {
        if !valid_name(name) {
            return Err(Error::Invalid);
        }
        let (commands, receiver) = channel();
        let (init, ready) = channel();
        let shared: Shared = Arc::new(Mutex::new(None));
        let (name, scratch): (String, PathBuf) = (name.to_string(), scratch.to_path_buf());
        let handle = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("hsearch-writer".into())
                .spawn(move || run(store, name, schema, scratch, hooks, shared, init, receiver))
                .map_err(|_| Error::Closed)?
        };
        match ready.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                shared,
                thread: Mutex::new(Some(handle)),
            }),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(error)
            }
            Err(_) => {
                let _ = handle.join();
                Err(Error::Closed)
            }
        }
    }

    fn ask<T>(&self, build: impl FnOnce(Sender<Result<T>>) -> Command) -> Result<T> {
        let (reply, answer) = channel();
        self.commands
            .send(build(reply))
            .map_err(|_| Error::Closed)?;
        answer.recv().map_err(|_| Error::Closed)?
    }

    /// Make `ops` durable as part of one log slot, shared with any bulks queued
    /// beside it, and return one result per operation.
    pub fn bulk(&self, ops: Vec<Op>, wait: Wait) -> Result<Vec<ItemResult>> {
        self.ask(|reply| Command::Bulk(Bulk { ops, wait, reply }))
    }

    /// Make everything applied so far searchable.
    pub fn refresh(&self) -> Result<()> {
        self.ask(Command::Refresh)
    }

    /// Commit the engine, upload new segment files and swap the root.
    pub fn publish(&self) -> Result<Published> {
        self.ask(Command::Publish)
    }

    /// The latest searchable snapshot.
    pub fn snapshot(&self) -> Result<Arc<Snapshot>> {
        self.shared
            .lock()
            .map_err(|_| Error::Poisoned)?
            .clone()
            .ok_or(Error::Busy)
    }

    /// Stop the writer without publishing.
    pub fn close(self) {
        drop(self);
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Ok(mut thread) = self.thread.lock()
            && let Some(handle) = thread.take()
        {
            let _ = handle.join();
        }
    }
}

/// Open the segments the root last published, read-only, through a cache at
/// `cache`. Needs no writer and replays nothing.
pub fn open_published<S: MetadataStore + 'static>(
    store: Arc<S>,
    name: &str,
    schema: Schema,
    cache: &Path,
) -> Result<Snapshot> {
    if !valid_name(name) {
        return Err(Error::Invalid);
    }
    let (root, _) = read_root(&*store, name)?;
    if root.files.is_empty() {
        return Err(Error::NotFound);
    }
    let names: Vec<String> = root.files.iter().map(|(n, _)| n.clone()).collect();
    let remote = StoreFiles {
        store,
        keys: root.files.into_iter().collect(),
    };
    Snapshot::open_remote(cache, remote, names, 256 << 20, schema).map_err(Error::from)
}
