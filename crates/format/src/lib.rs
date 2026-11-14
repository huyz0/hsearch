//! Versioned, bounded on-disk formats (M03.03). The byte layouts are specified in
//! `docs/plan/formats.md`; this crate is their only encoder and decoder.
//!
//! Rules every decoder here keeps:
//! - Every length and count is compared with its bound **and** with the bytes that remain
//!   before anything is allocated or sliced, so a forged length costs nothing.
//! - An unknown envelope version, kind, kind version or mandatory capability is refused,
//!   never skipped. Optional capabilities are ignored.
//! - Integrity is an embedded checksum or SHA-256 digest. A provider ETag or
//!   [`hsearch-object` version token] names a version; it is never a digest and no function
//!   here accepts one as such.
//! - Decoding is zero-copy where the result is read once (`SlotView`), and every encoder
//!   appends to a caller-owned buffer in one pass.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

mod codec;
mod crc;
mod envelope;
mod error;
mod fleet;
mod lane;
mod layout;
mod limits;
mod manifest;
mod mapping;
mod pin;

pub use crc::{crc32c, crc32c_bytewise};
pub use envelope::{Header, Kind, SUPPORTED_MANDATORY_CAPS, open, seal};
pub use error::{FormatError, Result};
pub use fleet::{FleetRecord, NodeSupport, Refusal};
pub use lane::{
    Durability, KeyIdentity, LaneDescriptor, PayloadRef, PolicyTuple, RecordOp, RecordRef,
    RecordSpec, SlotBody, SlotKind, SlotPosition, SlotView, aad, encode_slot,
};
pub use layout::Layout;
pub use limits::*;
pub use manifest::{FileEntry, ManifestPage, ManifestRoot, PageRef};
pub use mapping::MappingObject;
pub use pin::{Cursor, CursorBody, PinRecord};

/// A 128-bit opaque identity (index, lane domain, object, shard, pin).
pub type Id = [u8; 16];
/// A SHA-256 digest.
pub type Digest = [u8; 32];

/// A unique token carried inside every commit that must fence (M03.11).
///
/// RustFS 1.0.4 accepts a stale `If-Match` write whose bytes equal the current object, so
/// byte-equality of two versions must never be the only difference between a stale and a
/// current writer. The all-zero token is refused so an uninitialised field cannot pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitToken([u8; 16]);

impl CommitToken {
    /// Wrap caller-generated unique bytes; all-zero is refused.
    pub fn new(bytes: [u8; 16]) -> Result<Self> {
        if bytes == [0; 16] {
            return Err(FormatError::Malformed("commit token is zero"));
        }
        Ok(Self(bytes))
    }

    /// The raw token bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}
