//! Verified streaming upload of a native commit's files.
//!
//! Opening an index does not recompute file checksums (see [`crate::verify_files`]), so a file
//! must be proven intact before a manifest names it. [`upload_files`] verifies every named file's
//! footer checksum first, so one damaged file uploads nothing at all, then streams each file
//! through an [`ObjectUploadSink`] (create-only, at most one part buffered) while hashing the exact
//! bytes sent. Which keys and which manifest the files belong to is the caller's choice.
use std::io;
use std::sync::Arc;
use std::time::Duration;

use hsearch_object::{LargeObjectWriter, ObjectKey};
use lucene_store::{Directory, OutputSink};
use sha2::{Digest, Sha256};

use crate::ObjectUploadSink;

/// Bytes handed to the sink at a time.
const CHUNK: usize = 1 << 20;

/// One file that is now an immutable object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadedFile {
    /// The native file name.
    pub name: String,
    /// Bytes uploaded.
    pub length: u64,
    /// SHA-256 of exactly those bytes.
    pub digest: [u8; 32],
}

/// Why an upload stopped. Files uploaded before it are orphans for garbage collection to reclaim.
#[derive(Debug)]
pub enum PublishError {
    /// A named file is missing or fails its footer checksum; nothing was uploaded.
    Unverified,
    /// The caller's key function refused a name, or the part size is out of range.
    Key(String),
    /// The store refused or failed this file (for example the key already exists).
    Upload {
        /// The native file name.
        name: String,
        /// What kind of failure the sink reported.
        kind: io::ErrorKind,
    },
}

/// Verify every file in `names`, then upload each to `key_of(name)` and return what was uploaded,
/// in `names` order.
pub fn upload_files<D: Directory + ?Sized, B: LargeObjectWriter>(
    directory: &D,
    names: &[String],
    store: &Arc<B>,
    key_of: &dyn Fn(&str) -> Option<ObjectKey>,
    part_size: usize,
    deadline: Duration,
) -> Result<Vec<UploadedFile>, PublishError> {
    crate::verify_files(directory, names).map_err(|_| PublishError::Unverified)?;
    let mut uploaded = Vec::with_capacity(names.len());
    for name in names {
        let key = key_of(name).ok_or_else(|| PublishError::Key(name.clone()))?;
        let sink = ObjectUploadSink::new(Arc::clone(store), key, part_size, deadline)
            .map_err(|_| PublishError::Key(name.clone()))?;
        let input = directory.open(name).map_err(|_| PublishError::Unverified)?;
        let bytes: &[u8] = &input;
        let mut sink: Box<ObjectUploadSink<B>> = Box::new(sink);
        let mut hash = Sha256::new();
        for chunk in bytes.chunks(CHUNK) {
            hash.update(chunk);
            sink.write_all(chunk).map_err(|e| PublishError::Upload {
                name: name.clone(),
                kind: e.kind(),
            })?;
        }
        sink.finish().map_err(|e| PublishError::Upload {
            name: name.clone(),
            kind: e.kind(),
        })?;
        uploaded.push(UploadedFile {
            name: name.clone(),
            length: bytes.len() as u64,
            digest: hash.finalize().into(),
        });
    }
    Ok(uploaded)
}

/// [`upload_files`] over the native commit a [`crate::LocalFiles`] views.
pub fn upload_local<B: LargeObjectWriter>(
    files: &crate::LocalFiles,
    names: &[String],
    store: &Arc<B>,
    key_of: &dyn Fn(&str) -> Option<ObjectKey>,
    part_size: usize,
    deadline: Duration,
) -> Result<Vec<UploadedFile>, PublishError> {
    let directory = lucene_store::FsDirectory::open(files.root());
    upload_files(&directory, names, store, key_of, part_size, deadline)
}
