//! The durable garbage-collection boundary: one generation per shard incarnation that only rises.
//!
//! Collection publishes the boundary **before** it scans pins, and a reader re-reads it **after**
//! it registers a pin; a generation below the boundary is never handed out. Together the two
//! orders close the race the `pin_gc` model searched exhaustively (a pin registered after the
//! scan is refused by the boundary the scan was preceded by).
use hsearch_format::{Id, Layout};
use hsearch_object::{Error as StoreError, MetadataStore, Precondition, VersionToken};
use sha2::{Digest as _, Sha256};

use crate::io::{Io, request_id};

const MAGIC: [u8; 5] = *b"HSGB\x01";
const LEN: usize = 5 + 8 + 8;

/// Why a boundary could not be read or raised.
#[derive(Debug)]
pub enum Error {
    /// The stored boundary is not a boundary this build wrote.
    Corrupt,
    /// The store failed.
    Store(StoreError),
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

fn encode(generation: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(LEN);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&generation.to_le_bytes());
    let digest = Sha256::digest(&out);
    out.extend_from_slice(&digest[..8]);
    out
}

fn decode(bytes: &[u8]) -> Result<u64, Error> {
    let (body, check) = bytes.split_at_checked(LEN - 8).ok_or(Error::Corrupt)?;
    let digest = Sha256::digest(body);
    if bytes.len() != LEN || check != &digest[..8] {
        return Err(Error::Corrupt);
    }
    let (magic, value) = body.split_at(MAGIC.len());
    if magic != MAGIC {
        return Err(Error::Corrupt);
    }
    let value: [u8; 8] = value.try_into().map_err(|_| Error::Corrupt)?;
    Ok(u64::from_le_bytes(value))
}

/// The boundary of one shard incarnation and where it is stored.
pub(crate) struct Boundary {
    key: String,
}

impl Boundary {
    pub(crate) fn new(layout: &Layout, index: &Id, shard: &Id, incarnation: u64) -> Self {
        Self {
            key: layout.gc_boundary(index, shard, incarnation),
        }
    }

    /// The boundary and the version it was read at; 0 and `None` before the first collection.
    pub(crate) fn read<S: MetadataStore>(
        &self,
        io: &Io<S>,
    ) -> Result<(u64, Option<VersionToken>), Error> {
        match io.read(&self.key)? {
            None => Ok((0, None)),
            Some((bytes, version)) => Ok((decode(&bytes)?, Some(version))),
        }
    }

    /// Raise the boundary to at least `to` and return what it is now. It never falls, so a lost race
    /// to a higher value is success, and a lost race to a lower one is retried (bounded).
    pub(crate) fn raise<S: MetadataStore>(&self, io: &Io<S>, to: u64) -> Result<u64, Error> {
        for _ in 0..8 {
            let (now, version) = self.read(io)?;
            if now >= to {
                return Ok(now);
            }
            let precondition = match version.clone() {
                None => Precondition::Absent,
                Some(v) => Precondition::Matches(v),
            };
            let identity = request_id(&[
                b"boundary",
                self.key.as_bytes(),
                version.as_ref().map_or(&[][..], VersionToken::as_bytes),
                &to.to_le_bytes(),
            ])?;
            match io.write(&self.key, &encode(to), precondition, identity) {
                Ok(()) => return Ok(to),
                // Another collector moved it: read again and decide again.
                Err(StoreError::Conflict) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Store(StoreError::Conflict))
    }
}
