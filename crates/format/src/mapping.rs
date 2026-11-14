//! Mapping objects keyed by content hash (PR-43). The hash is SHA-256 over the caller's
//! canonical mapping bytes, never over the envelope and never a provider ETag.
use crate::codec::{Reader, Writer};
use crate::envelope::{Kind, open};
use crate::limits::MAX_MAPPING_BYTES;
use crate::{Digest, FormatError, Result};
use sha2::{Digest as _, Sha256};

/// An immutable, shareable mapping. Canonicalisation (key order, defaults) is the mapping
/// owner's job; this type only stores and addresses the canonical bytes it is given.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MappingObject {
    /// SHA-256 of `canonical`.
    pub hash: Digest,
    /// Canonical bytes, at most [`MAX_MAPPING_BYTES`].
    pub canonical: Vec<u8>,
}

impl MappingObject {
    /// Hash `canonical` and build the object.
    pub fn new(canonical: &[u8]) -> Result<Self> {
        if canonical.is_empty() || canonical.len() > MAX_MAPPING_BYTES {
            return Err(FormatError::TooLarge("mapping"));
        }
        Ok(Self {
            hash: Sha256::digest(canonical).into(),
            canonical: canonical.to_vec(),
        })
    }

    /// Append the encoded object to `out`; the hash is recomputed, not trusted.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if Digest::from(Sha256::digest(&self.canonical)) != self.hash || self.canonical.is_empty() {
            return Err(FormatError::Checksum);
        }
        let mut w = Writer::begin(out, Kind::Mapping, 1, 0);
        w.bytes(&self.hash);
        w.bytes(&self.canonical);
        w.finish()
    }

    /// Decode one object and verify the stored hash against the bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::Mapping)?;
        let mut r = Reader::new(body);
        let hash: Digest = r.array()?;
        let canonical = r.take(r.remaining())?;
        if canonical.is_empty() || canonical.len() > MAX_MAPPING_BYTES {
            return Err(FormatError::TooLarge("mapping"));
        }
        if Digest::from(Sha256::digest(canonical)) != hash {
            return Err(FormatError::Checksum);
        }
        Ok(Self {
            hash,
            canonical: canonical.to_vec(),
        })
    }

    /// Decode and require the hash the referrer named (a root's `mapping`, or the key suffix).
    pub fn decode_expecting(bytes: &[u8], expected: &Digest) -> Result<Self> {
        let m = Self::decode(bytes)?;
        if &m.hash != expected {
            return Err(FormatError::Mismatch("mapping hash"));
        }
        Ok(m)
    }
}
