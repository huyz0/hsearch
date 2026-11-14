//! Lane identity (M03.09) and the ordered slots a lane commits through.
use crate::codec::{Reader, Writer};
use crate::envelope::{Kind, open};
use crate::limits::*;
use crate::{CommitToken, Digest, FormatError, Id, Result};

/// Which key wraps the lane. A revocable key never wraps a shared lane (M03.09 decision 5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyIdentity {
    /// The deployment-default key; the common small index shares a lane on it.
    DeploymentDefault,
    /// A per-index customer key; its lane has at most one member.
    Byok(String),
}

/// Durability profile of the lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Durability {
    /// Object store only.
    Object = 1,
    /// Journal quorum for the unarchived tail.
    Quorum = 2,
}

/// Indexes may share a lane only when this whole tuple is equal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyTuple {
    /// Key identity.
    pub key: KeyIdentity,
    /// Residency or region label.
    pub residency: String,
    /// Durability profile.
    pub durability: Durability,
    /// Format version the lane's records are written in.
    pub format_version: u16,
}

/// The lane's structural record: policy, group id, ceiling and member set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaneDescriptor {
    /// Starts at 1, one higher per membership change.
    pub generation: u64,
    /// The lane domain this descriptor belongs to (also in its key).
    pub domain: Id,
    /// The identical-policy rule's tuple.
    pub policy: PolicyTuple,
    /// Identity of the group within its policy.
    pub group_id: Id,
    /// Group-size ceiling, at most [`MAX_LANE_GROUP`]; enforced when an index joins.
    pub group_ceiling: u32,
    /// Member index ids, strictly ascending so equal sets have equal bytes.
    pub members: Vec<Id>,
    /// Unique per commit (M03.11).
    pub token: CommitToken,
}

impl LaneDescriptor {
    fn check(&self) -> Result<()> {
        let residency_ok = !self.policy.residency.is_empty()
            && self
                .policy
                .residency
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
        if !residency_ok || self.policy.format_version == 0 {
            return Err(FormatError::Malformed("residency or format version"));
        }
        if self.generation == 0 || self.group_ceiling == 0 || self.group_ceiling > MAX_LANE_GROUP {
            return Err(FormatError::Malformed("lane generation or ceiling"));
        }
        if self.members.len() > self.group_ceiling as usize {
            return Err(FormatError::TooLarge("lane members"));
        }
        if self.members.windows(2).any(|w| w.first() >= w.get(1)) {
            return Err(FormatError::Malformed("members not strictly ascending"));
        }
        match &self.policy.key {
            KeyIdentity::Byok(id) if id.is_empty() || self.members.len() > 1 => Err(
                FormatError::Malformed("a BYOK key must not wrap a shared lane"),
            ),
            _ => Ok(()),
        }
    }

    /// Append the encoded descriptor to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        self.check()?;
        let mut w = Writer::begin(out, Kind::LaneDescriptor, 1, 0);
        w.u64(self.generation);
        w.bytes(&self.domain);
        let r = match &self.policy.key {
            KeyIdentity::DeploymentDefault => {
                w.u8(1);
                w.long_str("", MAX_KEY_ID_BYTES, "key")
            }
            KeyIdentity::Byok(id) => {
                w.u8(2);
                w.long_str(id, MAX_KEY_ID_BYTES, "key")
            }
        };
        let r = r.and_then(|()| w.short_str(&self.policy.residency, MAX_NAME_BYTES, "residency"));
        if let Err(e) = r {
            return w.abort(e);
        }
        w.u8(self.policy.durability as u8);
        w.u16(self.policy.format_version);
        w.bytes(&self.group_id);
        w.u32(self.group_ceiling);
        if let Err(e) = w.count(self.members.len(), MAX_LANE_GROUP as usize, "lane members") {
            return w.abort(e);
        }
        for m in &self.members {
            w.bytes(m);
        }
        w.bytes(self.token.as_bytes());
        w.finish()
    }

    /// Decode and validate one whole object.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::LaneDescriptor)?;
        let mut r = Reader::new(body);
        let generation = r.u64()?;
        let domain = r.id()?;
        let key_kind = r.u8()?;
        let key = r.long_str(MAX_KEY_ID_BYTES, "key")?;
        let key = match key_kind {
            1 if key.is_empty() => KeyIdentity::DeploymentDefault,
            2 => KeyIdentity::Byok(key.to_owned()),
            _ => return Err(FormatError::Malformed("key identity")),
        };
        let residency = r.short_str(MAX_NAME_BYTES, "residency")?.to_owned();
        let durability = match r.u8()? {
            1 => Durability::Object,
            2 => Durability::Quorum,
            _ => return Err(FormatError::Malformed("durability")),
        };
        let format_version = r.u16()?;
        let group_id = r.id()?;
        let group_ceiling = r.u32()?;
        let n = r.count(MAX_LANE_GROUP as usize, 16, "lane members")?;
        let mut members = Vec::with_capacity(n);
        for _ in 0..n {
            members.push(r.id()?);
        }
        let token = CommitToken::new(r.array()?)?;
        r.finish()?;
        let d = Self {
            generation,
            domain,
            policy: PolicyTuple {
                key,
                residency,
                durability,
                format_version,
            },
            group_id,
            group_ceiling,
            members,
            token,
        };
        d.check()?;
        Ok(d)
    }

    /// Decode and require the domain the key named, so a descriptor copied under another
    /// lane's key is refused.
    pub fn decode_at(bytes: &[u8], domain: &Id) -> Result<Self> {
        let d = Self::decode(bytes)?;
        if &d.domain != domain {
            return Err(FormatError::Mismatch("lane domain"));
        }
        Ok(d)
    }

    /// May an index with `policy` join this lane? Only on an identical tuple, below the ceiling.
    #[must_use]
    pub fn can_host(&self, policy: &PolicyTuple) -> bool {
        let room = match self.policy.key {
            KeyIdentity::Byok(_) => self.members.is_empty(),
            KeyIdentity::DeploymentDefault => self.members.len() < self.group_ceiling as usize,
        };
        &self.policy == policy && room
    }

    /// The successor descriptor with `index` added: a membership change, new token, next generation.
    pub fn with_member(&self, index: Id, token: CommitToken) -> Result<Self> {
        let Err(at) = self.members.binary_search(&index) else {
            return Err(FormatError::Malformed("already a member"));
        };
        if token == self.token {
            return Err(FormatError::Malformed("commit token reused"));
        }
        let mut next = self.clone();
        next.generation = self
            .generation
            .checked_add(1)
            .ok_or(FormatError::TooLarge("generation"))?;
        next.token = token;
        next.members.insert(at, index);
        next.check()?;
        Ok(next)
    }
}

/// What a record does to a document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordOp {
    /// Create or replace; carries a payload.
    Put = 1,
    /// Remove; carries none.
    Delete = 2,
}

/// Where a record's document bytes are.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadRef<'a> {
    /// No payload (delete).
    None,
    /// Up to [`MAX_INLINE_RECORD_BYTES`] inside the slot.
    Inline(&'a [u8]),
    /// A document up to [`MAX_DOCUMENT_BYTES`] in its own object, named by id and digest.
    External {
        /// The payload object.
        object: Id,
        /// Its length in bytes, at most [`MAX_DOCUMENT_BYTES`].
        length: u64,
        /// SHA-256 of the payload bytes.
        digest: Digest,
    },
}

/// One record of a data slot, borrowed from the slot bytes on decode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordSpec<'a> {
    /// The index this record belongs to; also bound into the record's AAD.
    pub index: Id,
    /// Per-index sequence of this record.
    pub seq: u64,
    /// The operation.
    pub op: RecordOp,
    /// The document bytes or their location.
    pub payload: PayloadRef<'a>,
}

/// A decoded record; the same shape the encoder takes.
pub type RecordRef<'a> = RecordSpec<'a>;

/// Whether a slot carries data or seals the lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotKind {
    /// Committed records.
    Data,
    /// A successor's fence: nothing after it may commit in this activation.
    Fence,
}

/// What to write into a slot.
#[derive(Clone, Copy, Debug)]
pub enum SlotBody<'a> {
    /// One to [`MAX_SLOT_RECORDS`] records.
    Data(&'a [RecordSpec<'a>]),
    /// A fence naming the successor's epoch.
    Fence {
        /// The successor's epoch.
        successor_epoch: u64,
    },
}

const AAD_LABEL: &[u8; 16] = b"hs-lane-record\x00\x01";
const AAD_LEN: usize = 16 + 16 + 8 + 4 + 8 + 4 + 16;

/// The AEAD associated data for one record: it binds the lane domain, activation, lane number, ordinal,
/// position and the **index id**, so a record cannot be replayed into another slot or index.
#[must_use]
pub fn aad(
    domain: &Id,
    activation: u64,
    lane: u32,
    ordinal: u64,
    record: u32,
    index: &Id,
) -> [u8; AAD_LEN] {
    let mut a = [0u8; AAD_LEN];
    let mut at = 0;
    for part in [
        &AAD_LABEL[..],
        domain,
        &activation.to_le_bytes(),
        &lane.to_le_bytes(),
        &ordinal.to_le_bytes(),
        &record.to_le_bytes(),
        index,
    ] {
        if let Some(dst) = a.get_mut(at..at + part.len()) {
            dst.copy_from_slice(part);
        }
        at += part.len();
    }
    a
}

fn put_record(w: &mut Writer<'_>, r: &RecordSpec<'_>) -> Result<()> {
    w.bytes(&r.index);
    w.u64(r.seq);
    w.u8(r.op as u8);
    match (r.op, r.payload) {
        (RecordOp::Delete, PayloadRef::None) => w.u8(0),
        (RecordOp::Put, PayloadRef::Inline(b))
            if !b.is_empty() && b.len() <= MAX_INLINE_RECORD_BYTES =>
        {
            w.u8(1);
            w.u32(u32::try_from(b.len()).map_err(|_| FormatError::TooLarge("inline record"))?);
            w.bytes(b);
        }
        (RecordOp::Put, PayloadRef::Inline(b)) if !b.is_empty() => {
            return Err(FormatError::TooLarge("inline record"));
        }
        (
            RecordOp::Put,
            PayloadRef::External {
                object,
                length,
                digest,
            },
        ) if (1..=MAX_DOCUMENT_BYTES).contains(&length) => {
            w.u8(2);
            w.bytes(&object);
            w.u64(length);
            w.bytes(&digest);
        }
        (RecordOp::Put, PayloadRef::External { .. }) => {
            return Err(FormatError::TooLarge("document"));
        }
        _ => return Err(FormatError::Malformed("record op and payload disagree")),
    }
    Ok(())
}

fn get_record<'a>(r: &mut Reader<'a>) -> Result<RecordSpec<'a>> {
    let index = r.id()?;
    let seq = r.u64()?;
    let op = match r.u8()? {
        1 => RecordOp::Put,
        2 => RecordOp::Delete,
        _ => return Err(FormatError::Malformed("record op")),
    };
    let payload = match (op, r.u8()?) {
        (RecordOp::Delete, 0) => PayloadRef::None,
        (RecordOp::Put, 1) => {
            let n =
                usize::try_from(r.u32()?).map_err(|_| FormatError::TooLarge("inline record"))?;
            if n == 0 || n > MAX_INLINE_RECORD_BYTES {
                return Err(FormatError::TooLarge("inline record"));
            }
            PayloadRef::Inline(r.take(n)?)
        }
        (RecordOp::Put, 2) => {
            let object = r.id()?;
            let length = r.u64()?;
            if length == 0 || length > MAX_DOCUMENT_BYTES {
                return Err(FormatError::TooLarge("document"));
            }
            PayloadRef::External {
                object,
                length,
                digest: r.array()?,
            }
        }
        _ => return Err(FormatError::Malformed("record payload")),
    };
    Ok(RecordSpec {
        index,
        seq,
        op,
        payload,
    })
}

/// Where a slot sits: lane domain, activation, lane number and ordinal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlotPosition {
    /// Opaque lane domain.
    pub domain: Id,
    /// Activation (writer incarnation) of the lane.
    pub activation: u64,
    /// Lane number within the domain.
    pub lane: u32,
    /// Position in the lane, from zero.
    pub ordinal: u64,
}

/// Append one slot to `out`. A record larger than its bound, or a slot larger than
/// [`MAX_SLOT_BODY_BYTES`], is refused and `out` is left as it was.
pub fn encode_slot(
    out: &mut Vec<u8>,
    at: &SlotPosition,
    token: CommitToken,
    body: &SlotBody<'_>,
) -> Result<()> {
    let mut w = Writer::begin(out, Kind::LaneSlot, 1, 0);
    w.bytes(&at.domain);
    w.u64(at.activation);
    w.u32(at.lane);
    w.u64(at.ordinal);
    w.bytes(token.as_bytes());
    match body {
        SlotBody::Fence { successor_epoch } => {
            w.u8(2);
            w.u64(*successor_epoch);
        }
        SlotBody::Data(records) => {
            w.u8(1);
            if records.is_empty() {
                return w.abort(FormatError::Malformed("empty data slot"));
            }
            if let Err(e) = w.count(records.len(), MAX_SLOT_RECORDS, "slot records") {
                return w.abort(e);
            }
            for rec in *records {
                if let Err(e) = put_record(&mut w, rec) {
                    return w.abort(e);
                }
            }
        }
    }
    w.finish()
}

/// A validated slot. Records borrow the input; iterating allocates nothing.
#[derive(Clone, Copy, Debug)]
pub struct SlotView<'a> {
    /// Where the slot sits.
    pub position: SlotPosition,
    /// The commit token (M03.11).
    pub token: CommitToken,
    /// Data or fence.
    pub kind: SlotKind,
    /// For a fence, the successor's epoch.
    pub successor_epoch: Option<u64>,
    count: usize,
    records: &'a [u8],
}

impl<'a> SlotView<'a> {
    /// [`SlotView::parse`], then require the position its key named, so a slot copied to another
    /// key is refused.
    pub fn parse_at(bytes: &'a [u8], at: &SlotPosition) -> Result<Self> {
        let v = Self::parse(bytes)?;
        if &v.position != at {
            return Err(FormatError::Mismatch("slot position"));
        }
        Ok(v)
    }

    /// Validate a whole slot object: envelope, header and **every record**, before any is yielded.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::LaneSlot)?;
        let mut r = Reader::new(body);
        let position = SlotPosition {
            domain: r.id()?,
            activation: r.u64()?,
            lane: r.u32()?,
            ordinal: r.u64()?,
        };
        let token = CommitToken::new(r.array()?)?;
        match r.u8()? {
            2 => {
                let epoch = r.u64()?;
                r.finish()?;
                Ok(Self {
                    position,
                    token,
                    kind: SlotKind::Fence,
                    successor_epoch: Some(epoch),
                    count: 0,
                    records: &[],
                })
            }
            1 => {
                // Smallest record: id 16 + seq 8 + op 1 + kind 1.
                let count = r.count(MAX_SLOT_RECORDS, 26, "slot records")?;
                if count == 0 {
                    return Err(FormatError::Malformed("empty data slot"));
                }
                let records = r.take(r.remaining())?;
                let mut rr = Reader::new(records);
                for _ in 0..count {
                    get_record(&mut rr)?;
                }
                rr.finish()?;
                Ok(Self {
                    position,
                    token,
                    kind: SlotKind::Data,
                    successor_epoch: None,
                    count,
                    records,
                })
            }
            _ => Err(FormatError::Malformed("slot kind")),
        }
    }

    /// Number of records (zero for a fence).
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether there are no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The records in order. `parse` already validated them, so this cannot fail.
    pub fn records(&self) -> impl Iterator<Item = RecordRef<'a>> + use<'a> {
        let mut r = Reader::new(self.records);
        (0..self.count).map_while(move |_| get_record(&mut r).ok())
    }
}
