//! The handle callers hold: commands go to the writer thread, searches read the
//! latest published snapshot directly.
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use hsearch_engine::{Hit, MergeMode, Schema, Snapshot};
use hsearch_object::MetadataStore;

use crate::live::{self, Entry, Overlay};
use crate::store::{StoreFiles, valid_name};
use crate::writer::{
    Bulk, Command, Hooks, ItemResult, NoHooks, Published, Shared, Wait, create_root, read_root, run,
};
use crate::{Error, Op, Result};

/// An open index with its single writer.
pub struct Index {
    commands: Sender<Command>,
    shared: Shared,
    overlay: Overlay,
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
        Self::create_merging(store, name, schema, scratch, MergeMode::Inline)
    }

    /// [`Index::create`] with the engine's automatic merges run as `mode` says.
    pub fn create_merging<S: MetadataStore + 'static>(
        store: Arc<S>,
        name: &str,
        schema: Schema,
        scratch: &Path,
        mode: MergeMode,
    ) -> Result<Self> {
        if !valid_name(name) {
            return Err(Error::Invalid);
        }
        create_root(&*store, name)?;
        Self::open_merging(store, name, schema, scratch, mode)
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

    /// [`Index::open`] with the engine's automatic merges run as `mode` says:
    /// [`MergeMode::Background`] keeps them off the writer's refresh.
    pub fn open_merging<S: MetadataStore + 'static>(
        store: Arc<S>,
        name: &str,
        schema: Schema,
        scratch: &Path,
        mode: MergeMode,
    ) -> Result<Self> {
        Self::open_hooked(store, name, schema, scratch, Arc::new(NoHooks), mode)
    }

    /// [`Index::open`] with observers at the crash points, for fault tests.
    pub fn open_with<S: MetadataStore + 'static>(
        store: Arc<S>,
        name: &str,
        schema: Schema,
        scratch: &Path,
        hooks: Arc<dyn Hooks>,
    ) -> Result<Self> {
        Self::open_hooked(store, name, schema, scratch, hooks, MergeMode::Inline)
    }

    fn open_hooked<S: MetadataStore + 'static>(
        store: Arc<S>,
        name: &str,
        schema: Schema,
        scratch: &Path,
        hooks: Arc<dyn Hooks>,
        mode: MergeMode,
    ) -> Result<Self> {
        if !valid_name(name) {
            return Err(Error::Invalid);
        }
        let (commands, receiver) = channel();
        let (init, ready) = channel();
        let shared: Shared = Arc::new(Mutex::new(None));
        let overlay = live::overlay(schema.clone());
        let (name, scratch): (String, PathBuf) = (name.to_string(), scratch.to_path_buf());
        let handle = {
            let shared = Arc::clone(&shared);
            let overlay = Arc::clone(&overlay);
            std::thread::Builder::new()
                .name("hsearch-writer".into())
                .spawn(move || {
                    run(
                        store, name, schema, scratch, hooks, mode, shared, overlay, init, receiver,
                    )
                })
                .map_err(|_| Error::Closed)?
        };
        match ready.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                shared,
                overlay,
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

    /// Replace the index's schema with one that extends it: fields added to a live
    /// index ([`Schema::evolve`]). Runs on the writer thread between log slots, so
    /// every bulk is written under one schema or the other; documents already
    /// logged keep their meaning, with the added fields absent from them. Costs no
    /// object-store request: the caller publishes the new mapping.
    pub fn evolve(&self, schema: Schema) -> Result<()> {
        self.ask(|reply| Command::Evolve(schema, reply))
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

    /// Read documents by id. With `realtime`, each answer is the latest
    /// acknowledged write to that id whether or not a refresh has made it
    /// searchable (the owner's view: nothing acknowledged is ever older than
    /// what is returned); without it, each answer is the searchable snapshot's.
    /// One answer per id, in the order asked, `None` for a document that does not
    /// exist. `projection` names the stored fields to return, in the order wanted.
    ///
    /// Costs no object-store request however many ids are asked for: documents
    /// come from the owner's local state. A real-time read of an id whose recorded
    /// payload was dropped to keep memory bounded asks for one refresh first.
    pub fn get_many(
        &self,
        ids: &[&str],
        projection: &[&str],
        realtime: bool,
    ) -> Result<Vec<Option<Hit>>> {
        for attempt in 0..2 {
            if let Some(found) = self.read(ids, projection, realtime)? {
                return Ok(found);
            }
            if attempt == 0 {
                self.refresh()?;
            }
        }
        Err(Error::Busy)
    }

    /// One pass of [`Index::get_many`]; `None` when a refresh is needed first.
    fn read(
        &self,
        ids: &[&str],
        projection: &[&str],
        realtime: bool,
    ) -> Result<Option<Vec<Option<Hit>>>> {
        // The record and the snapshot are taken together, in the writer's lock
        // order: a refresh cannot empty the record between the two.
        let (snapshot, entries, positions) = if realtime {
            let recent = live::lock(&self.overlay)?;
            if recent.poisoned() {
                return Err(Error::Poisoned);
            }
            let entries: Vec<Option<Entry>> = ids.iter().map(|id| recent.get(id)).collect();
            (self.snapshot(), entries, recent.positions(projection))
        } else {
            (self.snapshot(), vec![None; ids.len()], Vec::new())
        };
        let mut out = Vec::with_capacity(ids.len());
        for (id, entry) in ids.iter().zip(entries) {
            out.push(match entry {
                Some(Entry::Doc { version, fields }) => {
                    Some(live::hit(id, version, &fields, projection, &positions))
                }
                Some(Entry::Gone) => None,
                Some(Entry::Stale) => return Ok(None),
                // Only an id the record does not hold needs the snapshot, so a
                // refresh that left none does not stop a read it can answer.
                None => snapshot
                    .as_ref()
                    .map_err(|error| *error)?
                    .get(id, projection)?,
            });
        }
        Ok(Some(out))
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

/// Store the index's mapping bytes under a derived key. Fails with
/// [`Error::Exists`] if the index already has one. The caller owns the format.
pub fn put_mapping<S: MetadataStore + 'static>(store: &S, name: &str, bytes: &[u8]) -> Result<()> {
    if !valid_name(name) {
        return Err(Error::Invalid);
    }
    crate::writer::put_mapping(store, name, bytes)
}

/// Replace the mapping bytes stored for an index with `new` if they are still
/// `expected`. A compare-exchange: [`Error::Changed`] when another writer replaced
/// them first. Costs a constant number of requests, never one per document or
/// segment.
pub fn replace_mapping<S: MetadataStore + 'static>(
    store: &S,
    name: &str,
    expected: &[u8],
    new: &[u8],
) -> Result<()> {
    if !valid_name(name) {
        return Err(Error::Invalid);
    }
    crate::writer::replace_mapping(store, name, expected, new)
}

/// Read the mapping bytes stored for an index, or [`Error::NotFound`].
pub fn get_mapping<S: MetadataStore + 'static>(store: &S, name: &str) -> Result<Vec<u8>> {
    if !valid_name(name) {
        return Err(Error::Invalid);
    }
    crate::writer::get_mapping(store, name)
}

/// A shared key is exactly `v1/<deployment>/mappings/<64 lowercase hex>`. The
/// prefix `v1/` is reserved, so no index name can own it, and nothing else of
/// an index's keys can be named through this API.
fn valid_shared_key(key: &str) -> bool {
    let parts: Vec<&str> = key.split('/').collect();
    let [v1, deployment, kind, hash] = parts.as_slice() else {
        return false;
    };
    *v1 == "v1"
        && !deployment.is_empty()
        && deployment.len() <= 63
        && deployment
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && *kind == "mappings"
        && hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Create one immutable object shared by many indexes at `key` (for example a
/// mapping addressed by its content hash). Storing identical bytes again is a
/// success and writes nothing new; different bytes under the same key are
/// refused with [`Error::Corrupt`] and never replace what is there.
pub fn put_shared<S: MetadataStore + 'static>(store: &S, key: &str, bytes: &[u8]) -> Result<()> {
    if !valid_shared_key(key) {
        return Err(Error::Invalid);
    }
    crate::writer::put_shared(store, key, bytes)
}

/// Read a shared immutable object, or [`Error::NotFound`].
pub fn get_shared<S: MetadataStore + 'static>(store: &S, key: &str) -> Result<Vec<u8>> {
    if !valid_shared_key(key) {
        return Err(Error::Invalid);
    }
    crate::writer::get_shared(store, key)
}
