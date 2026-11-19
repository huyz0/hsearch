//! A scratch local directory owned for the life of one engine, for a writer
//! that rebuilds its state from a log. Nothing in it is authoritative: it is
//! cleared when opened and may be deleted at any time.
use std::path::{Path, PathBuf};

use lucene_store::{Directory, FsDirectory};

use crate::{Engine, EngineDirectory, Error, RemoteFiles, Result, Schema, Snapshot};

/// Read-only view of the native files an engine has committed locally.
pub struct LocalFiles {
    root: PathBuf,
}

impl LocalFiles {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Names of the native files present, excluding the write lock.
    pub fn names(&self) -> Result<Vec<String>> {
        let mut names: Vec<String> = FsDirectory::open(&self.root)
            .list_all()
            .map_err(|_| Error::Native)?
            .into_iter()
            .filter(|name| name != "write.lock")
            .collect();
        names.sort();
        Ok(names)
    }

    /// Write `checkpoint`'s tombstone list into the directory as [`crate::TOMBSTONES_FILE`] and
    /// return that name, to be published with the native files. Call after the commit it names and
    /// before the engine continues: it is the list for that commit only.
    pub fn write_tombstones(&self, checkpoint: &crate::Checkpoint) -> Result<String> {
        let bytes = checkpoint.tombstones()?;
        std::fs::write(self.root.join(crate::TOMBSTONES_FILE), bytes).map_err(|_| Error::Native)?;
        Ok(crate::TOMBSTONES_FILE.to_string())
    }

    /// The exact bytes of one native file.
    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        if name.is_empty() || name.contains(['/', '\\', '\0']) || name == ".." || name == "." {
            return Err(Error::Native);
        }
        std::fs::read(self.root.join(name)).map_err(|_| Error::Native)
    }
}

/// Open an engine on an emptied scratch directory at `root` and run `run`
/// with it. The engine is closed afterwards without committing; call
/// [`Engine::commit`] inside `run` for anything that must reach the files.
pub fn with_local_engine<R>(
    root: &Path,
    schema: Schema,
    run: impl FnOnce(&Engine<'_>, &LocalFiles) -> R,
) -> Result<R> {
    with_local_engine_merging(root, schema, crate::MergeMode::Inline, run)
}

/// [`with_local_engine`] with the automatic merges run as `mode` says. In
/// [`crate::MergeMode::Background`] one merge thread runs beside `run` and is
/// joined, with any merge it is running, before the engine closes.
pub fn with_local_engine_merging<R>(
    root: &Path,
    schema: Schema,
    mode: crate::MergeMode,
    run: impl FnOnce(&Engine<'_>, &LocalFiles) -> R,
) -> Result<R> {
    if root.exists() {
        std::fs::remove_dir_all(root).map_err(|_| Error::Native)?;
    }
    std::fs::create_dir_all(root).map_err(|_| Error::Native)?;
    let directory = EngineDirectory::new(FsDirectory::open(root));
    let engine = crate::EngineFactory::new().open_merging(
        &directory,
        schema,
        None,
        crate::WriterMode::Materialized,
        mode,
    )?;
    let files = LocalFiles {
        root: root.to_path_buf(),
    };
    let stop = std::sync::atomic::AtomicBool::new(false);
    // Stops the merge thread however `run` ends: the scope waits for it, so a
    // panic in `run` would otherwise wait forever.
    struct Stop<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    let result = std::thread::scope(|scope| {
        let _stop = Stop(&stop);
        if mode == crate::MergeMode::Background {
            scope.spawn(|| engine.run_background_merges(&stop));
        }
        run(&engine, &files)
    });
    engine.close()?;
    Ok(result)
}

/// Open an engine on the files of a base already placed in `root` (the native files and
/// [`crate::TOMBSTONES_FILE`], as a recovering writer fetched them) and run `run` with it. The
/// tombstone list is read and removed first, so the engine sees only native files; the engine is
/// closed afterwards without committing. A missing or invalid list, or one that does not belong to
/// these files, is `Error::Checkpoint`.
pub fn with_recovered_local_engine<R>(
    root: &Path,
    schema: Schema,
    mode: crate::MergeMode,
    run: impl FnOnce(&Engine<'_>, &LocalFiles) -> R,
) -> Result<R> {
    let list = root.join(crate::TOMBSTONES_FILE);
    let tombstones = std::fs::read(&list).map_err(|_| Error::Checkpoint)?;
    std::fs::remove_file(&list).map_err(|_| Error::Native)?;
    let directory = EngineDirectory::new(FsDirectory::open(root));
    let engine = Engine::open_recovered(&directory, schema, &tombstones, mode)?;
    let files = LocalFiles {
        root: root.to_path_buf(),
    };
    let result = run(&engine, &files);
    engine.close()?;
    Ok(result)
}

impl Snapshot {
    /// Open the committed segments named by `files` from `remote` through a
    /// bounded cache at `cache_root`, read-only. Costs one whole-file fetch per
    /// file and no LIST.
    pub fn open_remote<R: RemoteFiles>(
        cache_root: &Path,
        remote: R,
        files: Vec<String>,
        max_cache_bytes: u64,
        schema: Schema,
    ) -> Result<Self> {
        let cached = crate::CachedDirectory::new(cache_root, remote, files, max_cache_bytes)
            .map_err(|_| Error::Native)?;
        let directory = EngineDirectory::new(cached);
        Snapshot::open_committed(&directory, schema)
    }
}
