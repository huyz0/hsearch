//! Full-file integrity verification of committed native files.
//!
//! Opening an index checks each file's header and footer structure but, as in
//! Lucene, does not recompute every file's CRC-32 (that is `CheckIndex`'s job): a
//! flipped byte inside a points or doc-values file can open and answer wrongly.
//! [`verify_files`] recomputes the CRC of every named file against its footer. It
//! reads each file once, whole, so on a remote directory it costs one GET per
//! file, holds that file whole in memory while it checks it, and reports any failure,
//! missing file included, as `Error::Native`; run it where a file enters the store or leaves a repair, never per query.
use lucene_store::{Directory, codec_util};

use crate::{Error, Result};

/// Verify the Lucene footer checksum of every file in `names`. `Error::Native`
/// names no file: the caller already knows which it asked for, and the first
/// failing one stops the walk.
pub fn verify_files<D: Directory + ?Sized>(
    directory: &D,
    names: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<()> {
    for name in names {
        let input = directory.open(name.as_ref()).map_err(|_| Error::Native)?;
        let bytes: &[u8] = &input;
        let payload_end = bytes
            .len()
            .checked_sub(codec_util::FOOTER_LENGTH)
            .ok_or(Error::Native)?;
        codec_util::check_whole_file_footer(bytes, payload_end).map_err(|_| Error::Native)?;
    }
    Ok(())
}

/// [`verify_files`] over every file the directory lists except lock files.
pub fn verify_directory<D: Directory + ?Sized>(directory: &D) -> Result<()> {
    let names: Vec<String> = directory
        .list_all()
        .map_err(|_| Error::Native)?
        .into_iter()
        .filter(|name| !name.ends_with(".lock"))
        .collect();
    verify_files(directory, names)
}
