//! v0 log-slot and root encodings. Disposable: replaced by the M03.03 formats.
//! Every decode is bounds-checked and refuses rather than guesses.
use crate::{Error, Result};

const SLOT_MAGIC: &[u8; 8] = b"HSLOG0\0\0";
const ROOT_MAGIC: &[u8; 8] = b"HSROOT0\0";
/// Most operations one slot may carry.
pub const MAX_SLOT_OPS: usize = 4096;
/// Largest encoded slot, below the object contract's 16 MiB payload bound.
pub const MAX_SLOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_FIELD_BYTES: usize = 8192;
const MAX_FIELDS: usize = 16;
const MAX_FILES: usize = 4096;

/// One typed field value, in the schema's canonical order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Field {
    /// Keyword or text.
    Str(String),
    /// 32-bit integer value.
    Int(i64),
    /// Boolean.
    Bool(bool),
    /// UTC epoch milliseconds.
    Date(i64),
}

/// One mutation of a bulk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Create or replace a document with exactly the schema's fields.
    Put {
        /// Document identity.
        id: String,
        /// Field values in schema order.
        fields: Vec<Field>,
    },
    /// Remove a document.
    Delete {
        /// Document identity.
        id: String,
    },
}

impl Op {
    /// The document identity.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Put { id, .. } | Self::Delete { id } => id,
        }
    }
}

/// A committed group of operations at a position in the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    /// Position in the log, from zero.
    pub seq: u64,
    /// Version of the first operation; each later one is one higher.
    pub first_version: i64,
    /// The operations, in order.
    pub ops: Vec<Op>,
}

/// The published pointer: which slots the published segments cover, and which
/// files they are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    /// Writer incarnation, bumped by every open. Fences earlier writers.
    pub epoch: u64,
    /// Slots `0..covered` are contained in the published segments.
    pub covered: u64,
    /// Native file names and their object keys.
    pub files: Vec<(String, String)>,
}

fn put_str(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Encode a slot.
#[must_use]
pub fn encode_slot(slot: &Slot) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(SLOT_MAGIC);
    out.extend_from_slice(&slot.seq.to_le_bytes());
    out.extend_from_slice(&slot.first_version.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(slot.ops.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for op in &slot.ops {
        match op {
            Op::Put { id, fields } => {
                out.push(1);
                put_str(&mut out, id.as_bytes());
                out.push(u8::try_from(fields.len()).unwrap_or(u8::MAX));
                for field in fields {
                    match field {
                        Field::Str(s) => {
                            out.push(1);
                            put_str(&mut out, s.as_bytes());
                        }
                        Field::Int(n) => {
                            out.push(2);
                            out.extend_from_slice(&n.to_le_bytes());
                        }
                        Field::Bool(b) => {
                            out.push(3);
                            out.push(u8::from(*b));
                        }
                        Field::Date(n) => {
                            out.push(4);
                            out.extend_from_slice(&n.to_le_bytes());
                        }
                    }
                }
            }
            Op::Delete { id } => {
                out.push(2);
                put_str(&mut out, id.as_bytes());
            }
        }
    }
    out
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(Error::Corrupt)?;
        let slice = self.bytes.get(self.at..end).ok_or(Error::Corrupt)?;
        self.at = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| Error::Corrupt)?,
        ))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| Error::Corrupt)?,
        ))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| Error::Corrupt)?,
        ))
    }
    fn string(&mut self, max: usize) -> Result<String> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(Error::Corrupt);
        }
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| Error::Corrupt)
    }
    fn done(&self) -> Result<()> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::Corrupt)
        }
    }
}

/// Decode a slot, refusing anything that is not exactly a v0 slot.
pub fn decode_slot(bytes: &[u8]) -> Result<Slot> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(8)? != SLOT_MAGIC {
        return Err(Error::Corrupt);
    }
    let seq = r.u64()?;
    let first_version = r.i64()?;
    if first_version <= 0 {
        return Err(Error::Corrupt);
    }
    let count = r.u32()? as usize;
    if count == 0 || count > MAX_SLOT_OPS {
        return Err(Error::Corrupt);
    }
    let mut ops = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        match r.u8()? {
            1 => {
                let id = r.string(256)?;
                let n = r.u8()? as usize;
                if n == 0 || n > MAX_FIELDS {
                    return Err(Error::Corrupt);
                }
                let mut fields = Vec::with_capacity(n);
                for _ in 0..n {
                    fields.push(match r.u8()? {
                        1 => Field::Str(r.string(MAX_FIELD_BYTES)?),
                        2 => Field::Int(r.i64()?),
                        3 => match r.u8()? {
                            0 => Field::Bool(false),
                            1 => Field::Bool(true),
                            _ => return Err(Error::Corrupt),
                        },
                        4 => Field::Date(r.i64()?),
                        _ => return Err(Error::Corrupt),
                    });
                }
                ops.push(Op::Put { id, fields });
            }
            2 => ops.push(Op::Delete { id: r.string(256)? }),
            _ => return Err(Error::Corrupt),
        }
    }
    r.done()?;
    Ok(Slot {
        seq,
        first_version,
        ops,
    })
}

/// Encode a root.
#[must_use]
pub fn encode_root(root: &Root) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(ROOT_MAGIC);
    out.extend_from_slice(&root.epoch.to_le_bytes());
    out.extend_from_slice(&root.covered.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(root.files.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for (name, key) in &root.files {
        put_str(&mut out, name.as_bytes());
        put_str(&mut out, key.as_bytes());
    }
    out
}

/// Decode a root, refusing anything that is not exactly a v0 root.
pub fn decode_root(bytes: &[u8]) -> Result<Root> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(8)? != ROOT_MAGIC {
        return Err(Error::Corrupt);
    }
    let epoch = r.u64()?;
    let covered = r.u64()?;
    let count = r.u32()? as usize;
    if count > MAX_FILES {
        return Err(Error::Corrupt);
    }
    let mut files = Vec::with_capacity(count.min(256));
    for _ in 0..count {
        files.push((r.string(255)?, r.string(1024)?));
    }
    r.done()?;
    Ok(Root {
        epoch,
        covered,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> Slot {
        Slot {
            seq: 7,
            first_version: 41,
            ops: vec![
                Op::Put {
                    id: "a".into(),
                    fields: vec![
                        Field::Str("x".into()),
                        Field::Int(-3),
                        Field::Bool(true),
                        Field::Date(9),
                    ],
                },
                Op::Delete { id: "b".into() },
            ],
        }
    }

    #[test]
    fn slot_round_trips_and_every_truncation_is_refused() {
        let bytes = encode_slot(&slot());
        assert_eq!(decode_slot(&bytes).unwrap(), slot());
        for n in 0..bytes.len() {
            assert_eq!(decode_slot(&bytes[..n]), Err(Error::Corrupt), "cut at {n}");
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert_eq!(decode_slot(&extra), Err(Error::Corrupt));
        let mut wrong = bytes;
        wrong[0] ^= 1;
        assert_eq!(decode_slot(&wrong), Err(Error::Corrupt));
    }

    #[test]
    fn slot_refuses_bad_tags_counts_and_versions() {
        let mut bytes = encode_slot(&slot());
        bytes[8 + 8 + 8 + 4] = 9; // first op tag
        assert_eq!(decode_slot(&bytes), Err(Error::Corrupt));
        let mut zero_version = encode_slot(&slot());
        zero_version[16..24].copy_from_slice(&0i64.to_le_bytes());
        assert_eq!(decode_slot(&zero_version), Err(Error::Corrupt));
        let mut huge = encode_slot(&slot());
        huge[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_slot(&huge), Err(Error::Corrupt));
        let mut empty = encode_slot(&slot());
        empty[24..28].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode_slot(&empty), Err(Error::Corrupt));
    }

    #[test]
    fn root_round_trips_and_every_truncation_is_refused() {
        let root = Root {
            epoch: 3,
            covered: 12,
            files: vec![("_0.cfs".into(), "p/seg/3/_0.cfs".into())],
        };
        let bytes = encode_root(&root);
        assert_eq!(decode_root(&bytes).unwrap(), root);
        for n in 0..bytes.len() {
            assert_eq!(decode_root(&bytes[..n]), Err(Error::Corrupt), "cut at {n}");
        }
        let mut extra = bytes;
        extra.push(0);
        assert_eq!(decode_root(&extra), Err(Error::Corrupt));
    }
}
