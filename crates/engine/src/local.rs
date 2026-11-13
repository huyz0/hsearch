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
    if root.exists() {
        std::fs::remove_dir_all(root).map_err(|_| Error::Native)?;
    }
    std::fs::create_dir_all(root).map_err(|_| Error::Native)?;
    let directory = EngineDirectory::new(FsDirectory::open(root));
    let engine = Engine::open(&directory, schema, None)?;
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
