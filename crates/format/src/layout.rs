//! Object keys. Every key is derived from identities already in hand; nothing is discovered by
//! LIST. Ordinals are zero-padded so lexicographic order equals numeric order.
use crate::limits::{MAX_DEPLOYMENT_BYTES, MAX_KEY_BYTES};
use crate::{Digest, FormatError, Id, Result};
use std::fmt::Write as _;

/// Key builder for one deployment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    deployment: String,
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl Layout {
    /// A deployment name is 1 to 63 bytes of `a-z`, `0-9` and `-`, not starting or ending with `-`.
    pub fn new(deployment: &str) -> Result<Self> {
        let ok = !deployment.is_empty()
            && deployment.len() <= MAX_DEPLOYMENT_BYTES
            && !deployment.starts_with('-')
            && !deployment.ends_with('-')
            && deployment
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok {
            return Err(FormatError::Malformed("deployment name"));
        }
        Ok(Self {
            deployment: deployment.to_owned(),
        })
    }

    fn key(&self, tail: std::fmt::Arguments<'_>) -> String {
        let mut s = format!("v1/{}/", self.deployment);
        let _ = s.write_fmt(tail);
        debug_assert!(s.len() <= MAX_KEY_BYTES);
        s
    }

    /// The fleet format-version record.
    #[must_use]
    pub fn fleet(&self) -> String {
        self.key(format_args!("fleet/FORMAT"))
    }

    /// A lane's structural descriptor.
    #[must_use]
    pub fn lane_head(&self, domain: &Id, incarnation: u64) -> String {
        self.key(format_args!("lanes/{}/{incarnation:020}/HEAD", hex(domain)))
    }

    /// One ordered slot of a lane.
    #[must_use]
    pub fn lane_slot(
        &self,
        domain: &Id,
        incarnation: u64,
        activation: u64,
        lane: u32,
        ordinal: u64,
    ) -> String {
        self.key(format_args!(
            "lanes/{}/{incarnation:020}/wal/{activation:020}/{lane:010}/{ordinal:020}",
            hex(domain)
        ))
    }

    /// A payload object for a document too large to sit inline in a slot.
    #[must_use]
    pub fn payload(&self, index: &Id, object: &Id) -> String {
        self.key(format_args!(
            "indexes/{}/payloads/{}",
            hex(index),
            hex(object)
        ))
    }

    /// An index descriptor generation.
    #[must_use]
    pub fn index_descriptor(&self, index: &Id, generation: u64) -> String {
        self.key(format_args!(
            "indexes/{}/descriptor/{generation:020}",
            hex(index)
        ))
    }

    /// A shard's manifest root.
    #[must_use]
    pub fn manifest_root(&self, index: &Id, shard: &Id, incarnation: u64, manifest: &Id) -> String {
        self.key(format_args!(
            "indexes/{}/shards/{}/{incarnation:020}/manifests/{}",
            hex(index),
            hex(shard),
            hex(manifest)
        ))
    }

    /// A shard's manifest page.
    #[must_use]
    pub fn manifest_page(&self, index: &Id, shard: &Id, incarnation: u64, page: &Id) -> String {
        self.key(format_args!(
            "indexes/{}/shards/{}/{incarnation:020}/pages/{}",
            hex(index),
            hex(shard),
            hex(page)
        ))
    }

    /// A mapping, named by content hash. The name reveals equality of mappings to anyone who
    /// can list the bucket, which PR-43 accepts; principals reach it only via an index's descriptor.
    #[must_use]
    pub fn mapping(&self, hash: &Digest) -> String {
        self.key(format_args!("mappings/{}", hex(hash)))
    }

    /// A durable pin.
    #[must_use]
    pub fn pin(&self, index: &Id, pin: &Id) -> String {
        self.key(format_args!("indexes/{}/pins/{}", hex(index), hex(pin)))
    }
}
