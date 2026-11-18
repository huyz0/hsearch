//! The versions of deleted identities, persisted beside a base.
//!
//! A reopened index holds its live documents but not its tombstones: a delete leaves no
//! document, only a version the engine remembers so a later put at an older version is refused.
//! A writer recovered from a base would otherwise accept a stale put over a deleted identity. This
//! is the list of those versions at one commit, written with the base and read when the engine is
//! opened on it. It names the native commit it was taken at, so a list cannot be paired with files
//! of another commit: the engine refuses a mismatch. It is a bounded, length-checked list; anything that does not parse whole is
//! `Error::Checkpoint`, never a partial list.
use std::collections::BTreeMap;

use crate::adapter::Live;
use crate::{Error, Result};

const MAGIC: &[u8; 4] = b"HST2";
/// Most tombstones one base carries; a list past it is refused rather than truncated.
pub const MAX_TOMBSTONES: usize = 1 << 24;

/// The tombstones of `current`, in identity order.
pub(crate) fn encode(current: &BTreeMap<String, Live>, commit: [u8; 32]) -> Result<Vec<u8>> {
    let dead: Vec<(&String, i64)> = current
        .iter()
        .filter(|(_, live)| live.digest.is_none())
        .map(|(id, live)| (id, live.version))
        .collect();
    if dead.len() > MAX_TOMBSTONES {
        return Err(Error::Limit);
    }
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&commit);
    out.extend_from_slice(
        &u32::try_from(dead.len())
            .map_err(|_| Error::Limit)?
            .to_le_bytes(),
    );
    for (id, version) in dead {
        out.extend_from_slice(
            &u32::try_from(id.len())
                .map_err(|_| Error::Limit)?
                .to_le_bytes(),
        );
        out.extend_from_slice(id.as_bytes());
        out.extend_from_slice(&version.to_le_bytes());
    }
    Ok(out)
}

/// The commit a list was taken at and its identities and versions, refusing anything but a whole,
/// strictly ordered list.
pub(crate) fn decode(bytes: &[u8]) -> Result<([u8; 32], BTreeMap<String, i64>)> {
    let bad = Error::Checkpoint;
    let rest = bytes.strip_prefix(MAGIC).ok_or(bad)?;
    let (commit, rest) = rest.split_first_chunk::<32>().ok_or(bad)?;
    let (count, mut rest) = rest.split_first_chunk::<4>().ok_or(bad)?;
    let count = u32::from_le_bytes(*count) as usize;
    // Each entry is at least 4 + 1 + 8 bytes, so a count the bytes cannot hold is refused first.
    if count > MAX_TOMBSTONES || count.saturating_mul(13) > rest.len() {
        return Err(bad);
    }
    let mut out = BTreeMap::new();
    for _ in 0..count {
        let (len, r) = rest.split_first_chunk::<4>().ok_or(bad)?;
        let len = u32::from_le_bytes(*len) as usize;
        if len == 0 || len > 256 || r.len() < len + 8 {
            return Err(bad);
        }
        let (id, r) = r.split_at(len);
        let (version, r) = r.split_first_chunk::<8>().ok_or(bad)?;
        let version = i64::from_le_bytes(*version);
        let id = String::from_utf8(id.to_vec()).map_err(|_| bad)?;
        // Identity order, so a duplicate or a reordered list is one this engine never wrote.
        if version <= 0 || out.last_key_value().is_some_and(|(last, _)| *last >= id) {
            return Err(bad);
        }
        out.insert(id, version);
        rest = r;
    }
    if !rest.is_empty() {
        return Err(bad);
    }
    Ok((*commit, out))
}
