//! The structural head, the catalog root and catalog pages (M03.05).
//!
//! The head binds one commit domain's authority: activation, mode, epochs, the inventory of every
//! epoch that ever held the lane, the checkpoint cover and tail locator, and the schema and
//! publication generations. It changes on a structural transition, never per data append. The
//! catalog maps names to opaque index identities across a fixed fan-out of bounded pages; a name's
//! deletion is a retained tombstone, so a name never resolves to an identity it once lost.
use crate::codec::{Reader, Writer};
use crate::envelope::{Kind, open};
use crate::lane::Durability;
use crate::limits::{
    MAX_CATALOG_BUCKET_BITS, MAX_CATALOG_PAGE_ENTRIES, MAX_EPOCH_INVENTORY, MAX_INDEX_NAME_BYTES,
};
use crate::{CommitToken, FormatError, Id, Result};
use sha2::{Digest as _, Sha256};

/// Where a successor starts sealing: the lane and first ordinal that may still be absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TailLocator {
    /// Lane number.
    pub lane: u32,
    /// First ordinal a successor looks at; never below the head's `cover`.
    pub ordinal: u64,
}

/// One writer epoch that held (or holds) the lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EpochEntry {
    /// The epoch, strictly ascending in the inventory.
    pub epoch: u64,
    /// The activation the epoch wrote in.
    pub activation: u64,
    /// Its lane.
    pub lane: u32,
    /// The ordinal of the fence the epoch created at its own establishment.
    pub fence: u64,
}

/// The authoritative conditional record of one commit domain incarnation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuralHead {
    /// Starts at 1, one higher per structural transition.
    pub generation: u64,
    /// The domain (also in the key).
    pub domain: Id,
    /// The incarnation (also in the key); a head is never deleted while it exists.
    pub incarnation: u64,
    /// The activation currently authorised to commit.
    pub activation: u64,
    /// Durability mode of the activation.
    pub mode: Durability,
    /// Newest writer epoch, 0 before the first is established.
    pub writer_epoch: u64,
    /// Membership epoch.
    pub membership_epoch: u64,
    /// Schema generation.
    pub schema_generation: u64,
    /// Publication generation.
    pub publication_generation: u64,
    /// Slots below this are contained in `checkpoint`.
    pub cover: u64,
    /// The checkpoint root covering `cover`; present exactly when `cover` is non-zero.
    pub checkpoint: Option<Id>,
    /// Where a successor starts sealing.
    pub tail: TailLocator,
    /// Every epoch that held the lane, ascending, append-only.
    pub inventory: Vec<EpochEntry>,
    /// Unique per commit.
    pub token: CommitToken,
}

impl StructuralHead {
    fn check(&self) -> Result<()> {
        if self.generation == 0 || self.incarnation == 0 || self.activation == 0 {
            return Err(FormatError::Malformed(
                "head generation, incarnation or activation",
            ));
        }
        if self.inventory.len() > MAX_EPOCH_INVENTORY {
            return Err(FormatError::TooLarge("epoch inventory"));
        }
        if self
            .inventory
            .windows(2)
            .any(|w| match (w.first(), w.get(1)) {
                (Some(a), Some(b)) => a.epoch >= b.epoch || a.activation > b.activation,
                _ => false,
            })
        {
            return Err(FormatError::Malformed(
                "inventory epochs must strictly ascend",
            ));
        }
        match self.inventory.last() {
            None if self.writer_epoch != 0 => {
                return Err(FormatError::Malformed("writer epoch without an inventory"));
            }
            Some(last) if last.epoch != self.writer_epoch || last.activation != self.activation => {
                return Err(FormatError::Malformed(
                    "newest inventory entry is not the writer",
                ));
            }
            _ => {}
        }
        if self.checkpoint == Some([0; 16]) {
            return Err(FormatError::Malformed(
                "the zero id is not a checkpoint root",
            ));
        }
        if self.checkpoint.is_some() != (self.cover != 0) {
            return Err(FormatError::Malformed("checkpoint and cover disagree"));
        }
        if self.tail.ordinal < self.cover {
            return Err(FormatError::Malformed("tail below the cover"));
        }
        Ok(())
    }

    /// Append the encoded head to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        self.check()?;
        let mut w = Writer::begin(out, Kind::StructuralHead, 1, 0);
        w.u64(self.generation);
        w.bytes(&self.domain);
        w.u64(self.incarnation);
        w.u64(self.activation);
        w.u8(self.mode as u8);
        w.u64(self.writer_epoch);
        w.u64(self.membership_epoch);
        w.u64(self.schema_generation);
        w.u64(self.publication_generation);
        w.u64(self.cover);
        w.bytes(&self.checkpoint.unwrap_or([0; 16]));
        w.u32(self.tail.lane);
        w.u64(self.tail.ordinal);
        if let Err(e) = w.count(self.inventory.len(), MAX_EPOCH_INVENTORY, "epoch inventory") {
            return w.abort(e);
        }
        for e in &self.inventory {
            w.u64(e.epoch);
            w.u64(e.activation);
            w.u32(e.lane);
            w.u64(e.fence);
        }
        w.bytes(self.token.as_bytes());
        w.finish()
    }

    /// Decode and validate one whole object.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::StructuralHead)?;
        let mut r = Reader::new(body);
        let generation = r.u64()?;
        let domain = r.id()?;
        let incarnation = r.u64()?;
        let activation = r.u64()?;
        let mode = match r.u8()? {
            1 => Durability::Object,
            2 => Durability::Quorum,
            _ => return Err(FormatError::Malformed("head mode")),
        };
        let writer_epoch = r.u64()?;
        let membership_epoch = r.u64()?;
        let schema_generation = r.u64()?;
        let publication_generation = r.u64()?;
        let cover = r.u64()?;
        let checkpoint = r.id()?;
        let checkpoint = (checkpoint != [0; 16]).then_some(checkpoint);
        let tail = TailLocator {
            lane: r.u32()?,
            ordinal: r.u64()?,
        };
        let n = r.count(MAX_EPOCH_INVENTORY, 28, "epoch inventory")?;
        let mut inventory = Vec::with_capacity(n);
        for _ in 0..n {
            inventory.push(EpochEntry {
                epoch: r.u64()?,
                activation: r.u64()?,
                lane: r.u32()?,
                fence: r.u64()?,
            });
        }
        let token = CommitToken::new(r.array()?)?;
        r.finish()?;
        let head = Self {
            generation,
            domain,
            incarnation,
            activation,
            mode,
            writer_epoch,
            membership_epoch,
            schema_generation,
            publication_generation,
            cover,
            checkpoint,
            tail,
            inventory,
            token,
        };
        head.check()?;
        Ok(head)
    }

    /// Decode and require the domain and incarnation the key named, so a head copied under
    /// another key is refused.
    pub fn decode_at(bytes: &[u8], domain: &Id, incarnation: u64) -> Result<Self> {
        let head = Self::decode(bytes)?;
        if &head.domain != domain {
            return Err(FormatError::Mismatch("head domain"));
        }
        if head.incarnation != incarnation {
            return Err(FormatError::Mismatch("head incarnation"));
        }
        Ok(head)
    }

    /// Is `next` a legal successor? One generation up with a new token, same domain, incarnation
    /// and mode, every epoch, generation and the cover only rising, and the inventory only
    /// extended. This is what keeps head epochs from regressing even against a writer holding a
    /// stale copy: the conditional write checks the version, this checks the content.
    pub fn validate_successor(&self, next: &Self) -> Result<()> {
        let bad = |what| Err(FormatError::Malformed(what));
        if next.domain != self.domain || next.incarnation != self.incarnation {
            return bad("successor changes the domain or incarnation");
        }
        if next.generation != self.generation.saturating_add(1) {
            return bad("successor generation must be exactly one higher");
        }
        if next.token == self.token {
            return bad("commit token reused");
        }
        if next.mode != self.mode {
            return bad("successor changes the mode");
        }
        if next.activation < self.activation
            || next.writer_epoch < self.writer_epoch
            || next.membership_epoch < self.membership_epoch
            || next.schema_generation < self.schema_generation
            || next.publication_generation < self.publication_generation
            || next.cover < self.cover
            || next.tail.ordinal < self.tail.ordinal && next.activation == self.activation
        {
            return bad("an epoch, generation or the cover would fall");
        }
        if next.inventory.get(..self.inventory.len()) != Some(&self.inventory[..]) {
            return bad("the epoch inventory is append-only");
        }
        next.check()
    }
}

/// The immutable root of a tenant catalog: how many buckets its names spread over.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogRoot {
    /// The catalog's identity (also in its keys).
    pub catalog: Id,
    /// `2^bucket_bits` pages, `1..=MAX_CATALOG_BUCKET_BITS`.
    pub bucket_bits: u8,
    /// Unique per commit.
    pub token: CommitToken,
}

impl CatalogRoot {
    /// Append the encoded root to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.bucket_bits == 0 || self.bucket_bits > MAX_CATALOG_BUCKET_BITS {
            return Err(FormatError::Malformed("catalog bucket bits"));
        }
        let mut w = Writer::begin(out, Kind::CatalogRoot, 1, 0);
        w.bytes(&self.catalog);
        w.u8(self.bucket_bits);
        w.bytes(self.token.as_bytes());
        w.finish()
    }

    /// Decode and validate one whole object.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::CatalogRoot)?;
        let mut r = Reader::new(body);
        let catalog = r.id()?;
        let bucket_bits = r.u8()?;
        if bucket_bits == 0 || bucket_bits > MAX_CATALOG_BUCKET_BITS {
            return Err(FormatError::Malformed("catalog bucket bits"));
        }
        let token = CommitToken::new(r.array()?)?;
        r.finish()?;
        Ok(Self {
            catalog,
            bucket_bits,
            token,
        })
    }
}

/// The bucket a name lives in: the leading `bits` bits of `SHA-256(catalog || name)`.
#[must_use]
pub fn bucket_of(catalog: &Id, bits: u8, name: &str) -> u32 {
    let mut h = Sha256::new();
    h.update(catalog);
    h.update(name.as_bytes());
    let d = h.finalize();
    let top = u32::from_be_bytes([
        d.first().copied().unwrap_or(0),
        d.get(1).copied().unwrap_or(0),
        d.get(2).copied().unwrap_or(0),
        d.get(3).copied().unwrap_or(0),
    ]);
    let bits = u32::from(bits.clamp(1, 32));
    top >> (32 - bits)
}

/// Whether a name currently resolves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameState {
    /// The name resolves to `index`.
    Live = 1,
    /// The name was deleted; `index` is the identity it lost and may never return to.
    Deleted = 2,
}

/// One name's binding, or its tombstone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogEntry {
    /// 1 to [`MAX_INDEX_NAME_BYTES`] bytes of UTF-8 with no control character.
    pub name: String,
    /// Live or deleted.
    pub state: NameState,
    /// The opaque identity the name resolves to, or last resolved to.
    pub index: Id,
    /// 1 at first creation, one higher each time the name is deleted or rebound.
    pub name_generation: u64,
}

/// One bounded page of the catalog: the names of one bucket, sorted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogPage {
    /// The catalog's identity (also in the key).
    pub catalog: Id,
    /// The bucket (also in the key); every name hashes here.
    pub bucket: u32,
    /// Starts at 1, one higher per commit.
    pub generation: u64,
    /// Strictly ascending by name.
    pub entries: Vec<CatalogEntry>,
    /// Unique per commit.
    pub token: CommitToken,
}

fn name_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_INDEX_NAME_BYTES && !name.chars().any(char::is_control)
}

impl CatalogPage {
    fn check(&self, bits: u8) -> Result<()> {
        if bits == 0 || bits > MAX_CATALOG_BUCKET_BITS {
            return Err(FormatError::Malformed("catalog bucket bits"));
        }
        if self.generation == 0 || u64::from(self.bucket) >= 1u64 << bits {
            return Err(FormatError::Malformed("page generation or bucket"));
        }
        if self.entries.len() > MAX_CATALOG_PAGE_ENTRIES {
            return Err(FormatError::TooLarge("catalog entries"));
        }
        if self
            .entries
            .windows(2)
            .any(|w| match (w.first(), w.get(1)) {
                (Some(a), Some(b)) => a.name >= b.name,
                _ => false,
            })
        {
            return Err(FormatError::Malformed("names not strictly ascending"));
        }
        for e in &self.entries {
            if !name_ok(&e.name) || e.name_generation == 0 {
                return Err(FormatError::Malformed("catalog name or name generation"));
            }
            if bucket_of(&self.catalog, bits, &e.name) != self.bucket {
                return Err(FormatError::Malformed("name is in the wrong bucket"));
            }
        }
        Ok(())
    }

    /// Append the encoded page to `out`. `bits` is the root's fan-out.
    pub fn encode(&self, out: &mut Vec<u8>, bits: u8) -> Result<()> {
        self.check(bits)?;
        let mut w = Writer::begin(out, Kind::CatalogPage, 1, 0);
        w.bytes(&self.catalog);
        w.u32(self.bucket);
        w.u64(self.generation);
        if let Err(e) = w.count(
            self.entries.len(),
            MAX_CATALOG_PAGE_ENTRIES,
            "catalog entries",
        ) {
            return w.abort(e);
        }
        for e in &self.entries {
            if let Err(err) = w.short_str(&e.name, MAX_INDEX_NAME_BYTES.min(255), "name") {
                return w.abort(err);
            }
            w.u8(e.state as u8);
            w.bytes(&e.index);
            w.u64(e.name_generation);
        }
        w.bytes(self.token.as_bytes());
        w.finish()
    }

    /// Decode and validate one whole object under the root's fan-out.
    pub fn decode(bytes: &[u8], bits: u8) -> Result<Self> {
        let (_, body) = open(bytes, Kind::CatalogPage)?;
        let mut r = Reader::new(body);
        let catalog = r.id()?;
        let bucket = r.u32()?;
        let generation = r.u64()?;
        let n = r.count(MAX_CATALOG_PAGE_ENTRIES, 26, "catalog entries")?;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let name = r
                .short_str(MAX_INDEX_NAME_BYTES.min(255), "name")?
                .to_owned();
            let state = match r.u8()? {
                1 => NameState::Live,
                2 => NameState::Deleted,
                _ => return Err(FormatError::Malformed("name state")),
            };
            entries.push(CatalogEntry {
                name,
                state,
                index: r.id()?,
                name_generation: r.u64()?,
            });
        }
        let token = CommitToken::new(r.array()?)?;
        r.finish()?;
        let page = Self {
            catalog,
            bucket,
            generation,
            entries,
            token,
        };
        page.check(bits)?;
        Ok(page)
    }

    /// Decode and require the catalog and bucket the key named.
    pub fn decode_at(bytes: &[u8], bits: u8, catalog: &Id, bucket: u32) -> Result<Self> {
        let page = Self::decode(bytes, bits)?;
        if &page.catalog != catalog {
            return Err(FormatError::Mismatch("page catalog"));
        }
        if page.bucket != bucket {
            return Err(FormatError::Mismatch("page bucket"));
        }
        Ok(page)
    }

    /// Is `next` a legal successor? One generation up with a new token, no entry dropped, a name's
    /// generation never falling, a live name keeping its identity unless it was deleted first,
    /// and a deleted name coming back only under an identity it never had.
    pub fn validate_successor(&self, next: &Self) -> Result<()> {
        let bad = |what| Err(FormatError::Malformed(what));
        if next.catalog != self.catalog || next.bucket != self.bucket {
            return bad("successor moves the page");
        }
        if next.generation != self.generation.saturating_add(1) {
            return bad("successor generation must be exactly one higher");
        }
        if next.token == self.token {
            return bad("commit token reused");
        }
        for old in &self.entries {
            let Ok(at) = next
                .entries
                .binary_search_by(|e| e.name.as_str().cmp(&old.name))
            else {
                return bad("a name or tombstone was dropped");
            };
            let Some(new) = next.entries.get(at) else {
                return bad("a name or tombstone was dropped");
            };
            if new == old {
                continue;
            }
            if new.name_generation <= old.name_generation {
                return bad("a changed name must raise its generation");
            }
            match (old.state, new.state) {
                (NameState::Live, NameState::Live) => return bad("a live name cannot be rebound"),
                (NameState::Live, NameState::Deleted) if new.index != old.index => {
                    return bad("a tombstone keeps the identity it deleted");
                }
                (NameState::Deleted, NameState::Live) if new.index == old.index => {
                    return bad("a deleted name cannot resurrect its old identity");
                }
                (NameState::Deleted, NameState::Deleted) => return bad("already deleted"),
                _ => {}
            }
        }
        Ok(())
    }
}
