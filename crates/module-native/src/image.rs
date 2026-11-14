//! Staging: one read of the module file into a sealed in-memory file.
//!
//! The file is opened once, streamed through a fixed buffer into a `memfd` while
//! it is hashed, and the hash is compared with the pin. The library is then
//! loaded from the sealed copy, never from the path, so the bytes that were
//! hashed are the bytes that run (no check-then-use gap). Memory is the fixed
//! buffer; the library itself lives in the memfd, not on the heap.

use crate::LoadError;
use crate::elf;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

const CHUNK: usize = 64 * 1024;

/// A hashed, dependency-checked, sealed copy of a module file.
pub struct Staged {
    pub(crate) file: File,
}

#[cfg(target_os = "linux")]
impl Staged {
    /// Keeps the descriptor open for the life of the process. The dynamic loader
    /// names a library by the path it was given, and `/proc/self/fd/N` is reused
    /// once N is closed: a later module opened through the same number would be
    /// taken for the library already loaded under that name. A descriptor that is
    /// never closed is never reused, so every load is a distinct library. It is
    /// the same lifetime as the mapping, which is never unloaded either.
    pub(crate) fn keep_open(self) {
        std::mem::forget(self.file);
    }

    /// The path the dynamic loader opens: this process's own descriptor.
    pub(crate) fn load_path(&self) -> String {
        use std::os::fd::AsRawFd;
        format!("/proc/self/fd/{}", self.file.as_raw_fd())
    }
}

/// Reads `path` once, checks size, SHA-256 and `DT_NEEDED`, and seals the copy.
#[cfg(target_os = "linux")]
pub fn stage(path: &Path, pin: &[u8; 32], max_bytes: u64) -> Result<Staged, LoadError> {
    use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};
    let io = |_| LoadError::Io("cannot read the module file");
    let mut source = File::open(path).map_err(io)?;
    if !source.metadata().map_err(io)?.is_file() {
        return Err(LoadError::Io("the module path is not a regular file"));
    }
    let fd = memfd_create(
        "hsearch-module",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .map_err(|_| LoadError::Io("cannot create a sealed copy (memfd_create)"))?;
    let mut copy = File::from(fd);
    let (mut hasher, mut total, mut buf) = (Sha256::new(), 0u64, vec![0u8; CHUNK]);
    loop {
        let n = source.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max_bytes {
            return Err(LoadError::TooLarge);
        }
        hasher.update(&buf[..n]);
        copy.write_all(&buf[..n])
            .map_err(|_| LoadError::Io("cannot write the sealed copy"))?;
    }
    let digest: [u8; 32] = hasher.finalize().into();
    if digest != *pin {
        return Err(LoadError::HashMismatch);
    }
    fcntl_add_seals(
        &copy,
        SealFlags::SEAL | SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE,
    )
    .map_err(|_| LoadError::Io("cannot seal the copy"))?;
    // Scan the sealed bytes, which are the ones that will be mapped.
    let needed = elf::needed(&copy).map_err(|e| match e {
        elf::ElfError::NotSharedObject => {
            LoadError::BadImage("not a 64-bit little-endian shared object")
        }
        elf::ElfError::Malformed => LoadError::BadImage("malformed dynamic section"),
    })?;
    if let Some(bad) = needed.into_iter().find(|n| !elf::allowed_dependency(n)) {
        return Err(LoadError::DisallowedDependency(bad));
    }
    Ok(Staged { file: copy })
}

/// Native modules load on Linux only until the macOS copy-and-hash path is built
/// and qualified; refusing is safer than shipping an unexercised loader.
#[cfg(not(target_os = "linux"))]
pub fn stage(_path: &Path, _pin: &[u8; 32], _max_bytes: u64) -> Result<Staged, LoadError> {
    Err(LoadError::Unsupported("native modules load on Linux only"))
}
