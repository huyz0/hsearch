//! v0 log-slot and root encodings. Disposable: replaced by the M03.03 formats.
//! Every decode is bounds-checked and refuses rather than guesses.
use crate::{Error, Result};
use std::sync::Arc;

const SLOT_MAGIC: &[u8; 8] = b"HSLOG0\0\0";
const FENCE_MAGIC: &[u8; 8] = b"HSFENC0\0";
const ROOT_MAGIC: &[u8; 8] = b"HSROOT0\0";
const INTENT_MAGIC: &[u8; 8] = b"HSINT0\0\0";
/// Most chunk-write attempts one intent object records. Bounds its size
/// (36 bytes each, well under the object contract's payload bound) and the
/// memory the writer holds for them; past it the oldest are forgotten, which
/// leaks their chunks and never deletes one a slot might name.
pub const MAX_INTENT_ENTRIES: usize = 16_384;
/// Most operations one slot may carry.
pub const MAX_SLOT_OPS: usize = 4096;
/// Largest encoded slot, below the object contract's 16 MiB payload bound.
pub const MAX_SLOT_BYTES: usize = 8 * 1024 * 1024;
/// Largest document the writer accepts, in the engine's accounting (identity,
/// names and values): the product's 200 MiB hard ceiling (PR-40). A string over
/// [`INLINE_FIELD_MAX`] is stored as chunk objects, so the slot stays small.
pub const MAX_INDEXED_DOCUMENT_BYTES: usize = 200 << 20;

/// Most string text one slot may carry for a schema whose documents may be
/// `document_bytes`: one document's ceiling, never less than the slot's own byte
/// bound. Replay of a slot written under this rule holds at most this much text.
/// Slots written before the rule existed are not re-checked on replay.
#[must_use]
pub fn slot_text_cap(document_bytes: usize) -> usize {
    MAX_SLOT_BYTES.max(document_bytes)
}

/// Whether `add` more bytes of text fit under `cap` after `have`.
#[must_use]
pub fn text_fits(have: usize, add: usize, cap: usize) -> bool {
    have.checked_add(add).is_some_and(|total| total <= cap)
}
/// A field is bounded by its slot: decode checks lengths against the bytes that
/// remain, so a larger bound here would only be a second constant to keep equal.
const MAX_FIELD_BYTES: usize = MAX_SLOT_BYTES;
const MAX_FIELDS: usize = 16;
/// A string over this many bytes is not written into the slot: it goes to its own
/// objects and the slot holds a reference. Keeps a slot (one object, under the
/// 16 MiB payload bound) small however large a document is.
pub const INLINE_FIELD_MAX: usize = 1 << 20;
/// Bytes of one blob object: under the object contract's payload bound, so a
/// document of any size is a run of ordinary create-only objects.
pub const BLOB_CHUNK_BYTES: usize = 8 << 20;
const BLOB_TAG: u8 = 5;
/// Wire size of a reference: tag, epoch, attempt, chunk count, byte length.
const BLOB_REF_BYTES: usize = 1 + 8 + 8 + 4 + 8;

/// Where an externalised string lives: the writer epoch that wrote it, how many
/// chunk objects hold it and its total bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blob {
    /// Writer incarnation that created the chunks.
    pub epoch: u64,
    /// Unique to the write attempt, so a retry after a failed attempt in the
    /// same epoch never collides with that attempt's orphan chunks.
    pub attempt: u64,
    /// Number of chunk objects, each at most [`BLOB_CHUNK_BYTES`].
    pub chunks: u32,
    /// Total bytes of the string.
    pub len: u64,
}

/// The place of a reference in a slot, for keying its chunk objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobAt {
    /// Slot position in the log.
    pub seq: u64,
    /// Operation index within the slot.
    pub op: usize,
    /// Field index within the operation.
    pub field: usize,
}
/// Most segment files a root may name.
pub const MAX_FILES: usize = 4096;

/// One typed field value, in the schema's canonical order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Field {
    /// Keyword or text. Shared: an op's text is held once, by the op, the engine's document
    /// and its stored value, which matters for a very large text field.
    Str(Arc<String>),
    /// 32-bit integer value.
    Int(i64),
    /// Boolean.
    Bool(bool),
    /// UTC epoch milliseconds.
    Date(i64),
}

impl Field {
    /// A text field value, taking ownership of the string as it is (no copy).
    pub fn text(s: impl Into<String>) -> Self {
        Field::Str(Arc::new(s.into()))
    }
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

/// One recorded attempt to write the chunks of one string, made *before* any of
/// its chunks, so a crash between the chunks and the slot leaves a record from
/// which every chunk key is derived (no LIST).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntentEntry {
    /// Slot position the attempt was for.
    pub seq: u64,
    /// Writer incarnation.
    pub epoch: u64,
    /// The attempt number in the chunk keys.
    pub attempt: u64,
    /// Operation index within the slot.
    pub op: u32,
    /// Field index within the operation.
    pub field: u32,
    /// Number of chunk objects the attempt writes.
    pub chunks: u32,
}

impl IntentEntry {
    /// Where the attempt's strings sit in the slot.
    #[must_use]
    pub fn at(&self) -> BlobAt {
        BlobAt {
            seq: self.seq,
            op: self.op as usize,
            field: self.field as usize,
        }
    }
}

fn put_str(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Encode a slot with every string inline, for small slots and tests. A string
/// over [`INLINE_FIELD_MAX`] is refused (`Invalid`) rather than encoded to a size
/// [`encoded_len`] does not count.
pub fn encode_slot(slot: &Slot) -> Result<Vec<u8>> {
    let large = slot.ops.iter().any(|op| match op {
        Op::Put { fields, .. } => fields
            .iter()
            .any(|f| matches!(f, Field::Str(s) if s.len() > INLINE_FIELD_MAX)),
        Op::Delete { .. } => false,
    });
    if large {
        return Err(Error::Invalid);
    }
    encode_slot_with(slot, &mut |_, _, _| Ok(None))
}

/// Writes one large string (op index, field index, text) and returns its
/// reference, or `None` to keep it inline.
pub type Externalise<'a> = dyn FnMut(usize, usize, &str) -> Result<Option<Blob>> + 'a;

/// Encode a slot, offering each string over [`INLINE_FIELD_MAX`] to
/// `externalise(op, field, text)`: it stores the text and returns the [`Blob`] to
/// write in its place, or `None` to keep it inline. An error aborts the encoding.
pub fn encode_slot_with(slot: &Slot, externalise: &mut Externalise<'_>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(SLOT_MAGIC);
    out.extend_from_slice(&slot.seq.to_le_bytes());
    out.extend_from_slice(&slot.first_version.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(slot.ops.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for (op_at, op) in slot.ops.iter().enumerate() {
        match op {
            Op::Put { id, fields } => {
                out.push(1);
                put_str(&mut out, id.as_bytes());
                out.push(u8::try_from(fields.len()).unwrap_or(u8::MAX));
                for (field_at, field) in fields.iter().enumerate() {
                    match field {
                        Field::Str(s) => {
                            let blob = if s.len() > INLINE_FIELD_MAX {
                                externalise(op_at, field_at, s)?
                            } else {
                                None
                            };
                            if let Some(blob) = blob {
                                out.push(BLOB_TAG);
                                out.extend_from_slice(&blob.epoch.to_le_bytes());
                                out.extend_from_slice(&blob.attempt.to_le_bytes());
                                out.extend_from_slice(&blob.chunks.to_le_bytes());
                                out.extend_from_slice(&blob.len.to_le_bytes());
                            } else {
                                out.push(1);
                                put_str(&mut out, s.as_bytes());
                            }
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
    Ok(out)
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

/// Total bytes of string text in `ops`, externalised or not. Replay holds all of
/// a slot's text in memory at once, so this, not [`encoded_len`], bounds it.
#[must_use]
pub fn text_len(ops: &[Op]) -> usize {
    ops.iter()
        .map(|op| match op {
            Op::Put { id, fields } => {
                id.len()
                    + fields
                        .iter()
                        .map(|f| if let Field::Str(s) = f { s.len() } else { 0 })
                        .sum::<usize>()
            }
            Op::Delete { id } => id.len(),
        })
        .sum()
}

/// The length of a slot of `ops` with every string over [`INLINE_FIELD_MAX`]
/// externalised, computed without encoding or cloning them: the size checks of a
/// large document must not copy it.
#[must_use]
pub fn encoded_len(ops: &[Op]) -> usize {
    // Magic, sequence, first version, operation count.
    let header = SLOT_MAGIC.len() + 8 + 8 + 4;
    ops.iter().fold(header, |total, op| {
        total
            + match op {
                // Tag, identity (length + bytes), field count, then each field.
                Op::Put { id, fields } => {
                    1 + 4
                        + id.len()
                        + 1
                        + fields
                            .iter()
                            .map(|field| match field {
                                Field::Str(s) if s.len() > INLINE_FIELD_MAX => BLOB_REF_BYTES,
                                Field::Str(s) => 1 + 4 + s.len(),
                                Field::Int(_) | Field::Date(_) => 1 + 8,
                                Field::Bool(_) => 1 + 1,
                            })
                            .sum::<usize>()
                }
                Op::Delete { id } => 1 + 4 + id.len(),
            }
    })
}

/// The fence a successor creates at the first absent slot when it opens. It
/// occupies the position, so a late write from a predecessor is refused, and it
/// is kept: recovery counts it as one consumed position and applies nothing.
#[must_use]
pub fn encode_fence(seq: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(FENCE_MAGIC);
    out.extend_from_slice(&seq.to_le_bytes());
    out
}

/// Whether `bytes` is a fence rather than a slot, by its magic alone.
#[must_use]
pub fn is_fence(bytes: &[u8]) -> bool {
    bytes.starts_with(FENCE_MAGIC)
}

/// Decode a fence and return the position it claims, which must be exactly the
/// bytes [`encode_fence`] writes.
pub fn decode_fence(bytes: &[u8]) -> Result<u64> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(8)? != FENCE_MAGIC {
        return Err(Error::Corrupt);
    }
    let seq = r.u64()?;
    r.done()?;
    Ok(seq)
}

/// Decode a slot with every string inline, refusing anything that is not exactly
/// a v0 slot, including a reference to an externalised string.
pub fn decode_slot(bytes: &[u8]) -> Result<Slot> {
    decode_slot_with(bytes, &mut |_, _| Err(Error::Corrupt))
}

/// Decode a slot, resolving each reference with `resolve(place, blob)`, which
/// returns the string. A resolved length that differs from the reference is corrupt.
pub fn decode_slot_with(
    bytes: &[u8],
    resolve: &mut dyn FnMut(BlobAt, Blob) -> Result<String>,
) -> Result<Slot> {
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
    for op_at in 0..count {
        match r.u8()? {
            1 => {
                let id = r.string(256)?;
                let n = r.u8()? as usize;
                if n == 0 || n > MAX_FIELDS {
                    return Err(Error::Corrupt);
                }
                let mut fields = Vec::with_capacity(n);
                for field_at in 0..n {
                    fields.push(match r.u8()? {
                        1 => Field::Str(Arc::new(r.string(MAX_FIELD_BYTES)?)),
                        BLOB_TAG => {
                            let blob = Blob {
                                epoch: r.u64()?,
                                attempt: r.u64()?,
                                chunks: r.u32()?,
                                len: r.u64()?,
                            };
                            let text = resolve(
                                BlobAt {
                                    seq,
                                    op: op_at,
                                    field: field_at,
                                },
                                blob,
                            )?;
                            if text.len() as u64 != blob.len {
                                return Err(Error::Corrupt);
                            }
                            Field::Str(Arc::new(text))
                        }
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

/// Encode an intent: the attempts whose chunks may exist with no slot naming them.
#[must_use]
pub fn encode_intent(entries: &[IntentEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + entries.len() * 36);
    out.extend_from_slice(INTENT_MAGIC);
    out.extend_from_slice(
        &u32::try_from(entries.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for e in entries {
        out.extend_from_slice(&e.seq.to_le_bytes());
        out.extend_from_slice(&e.epoch.to_le_bytes());
        out.extend_from_slice(&e.attempt.to_le_bytes());
        out.extend_from_slice(&e.op.to_le_bytes());
        out.extend_from_slice(&e.field.to_le_bytes());
        out.extend_from_slice(&e.chunks.to_le_bytes());
    }
    out
}

/// Decode an intent, refusing anything that is not exactly a v0 intent.
pub fn decode_intent(bytes: &[u8]) -> Result<Vec<IntentEntry>> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(8)? != INTENT_MAGIC {
        return Err(Error::Corrupt);
    }
    let count = r.u32()? as usize;
    if count > MAX_INTENT_ENTRIES {
        return Err(Error::Corrupt);
    }
    let mut out = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let entry = IntentEntry {
            seq: r.u64()?,
            epoch: r.u64()?,
            attempt: r.u64()?,
            op: r.u32()?,
            field: r.u32()?,
            chunks: r.u32()?,
        };
        let max = MAX_INDEXED_DOCUMENT_BYTES.div_ceil(BLOB_CHUNK_BYTES);
        if entry.chunks == 0 || entry.chunks as usize > max {
            return Err(Error::Corrupt);
        }
        out.push(entry);
    }
    r.done()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u64) -> IntentEntry {
        IntentEntry {
            seq: n,
            epoch: 3,
            attempt: 40 + n,
            op: 2,
            field: 1,
            chunks: 2,
        }
    }

    #[test]
    fn intent_round_trips_and_every_truncation_is_refused() {
        let entries = vec![entry(0), entry(1), entry(7)];
        let bytes = encode_intent(&entries);
        assert_eq!(decode_intent(&bytes).unwrap(), entries);
        assert_eq!(decode_intent(&encode_intent(&[])).unwrap(), vec![]);
        for cut in 0..bytes.len() {
            assert_eq!(decode_intent(&bytes[..cut]), Err(Error::Corrupt), "{cut}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(decode_intent(&longer), Err(Error::Corrupt));
        let mut magic = bytes;
        magic[0] ^= 1;
        assert_eq!(decode_intent(&magic), Err(Error::Corrupt));
    }

    #[test]
    fn intent_refuses_impossible_counts_and_chunk_numbers() {
        let mut zero = entry(0);
        zero.chunks = 0;
        assert_eq!(decode_intent(&encode_intent(&[zero])), Err(Error::Corrupt));
        let mut huge = entry(0);
        huge.chunks = u32::try_from(MAX_INDEXED_DOCUMENT_BYTES / BLOB_CHUNK_BYTES).unwrap() + 1;
        assert_eq!(decode_intent(&encode_intent(&[huge])), Err(Error::Corrupt));
        let mut max = entry(0);
        max.chunks = u32::try_from(MAX_INDEXED_DOCUMENT_BYTES / BLOB_CHUNK_BYTES).unwrap();
        assert!(decode_intent(&encode_intent(&[max])).is_ok());
        let many: Vec<_> = (0..=MAX_INTENT_ENTRIES as u64).map(entry).collect();
        assert_eq!(decode_intent(&encode_intent(&many)), Err(Error::Corrupt));
        let at_bound: Vec<_> = (0..MAX_INTENT_ENTRIES as u64).map(entry).collect();
        assert_eq!(
            decode_intent(&encode_intent(&at_bound)).unwrap().len(),
            MAX_INTENT_ENTRIES
        );
    }

    fn slot() -> Slot {
        Slot {
            seq: 7,
            first_version: 41,
            ops: vec![
                Op::Put {
                    id: "a".into(),
                    fields: vec![
                        Field::text("x"),
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
    fn a_fence_round_trips_and_is_never_a_slot() {
        let bytes = encode_fence(7);
        assert!(is_fence(&bytes));
        assert_eq!(decode_fence(&bytes).unwrap(), 7);
        assert!(decode_slot(&bytes).is_err());
        assert!(!is_fence(&encode_slot(&slot()).unwrap()));
        for cut in 0..bytes.len() {
            assert!(decode_fence(&bytes[..cut]).is_err());
        }
        let mut long = bytes.clone();
        long.push(0);
        assert!(decode_fence(&long).is_err());
    }

    #[test]
    fn slot_round_trips_and_every_truncation_is_refused() {
        let bytes = encode_slot(&slot()).unwrap();
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
    fn encoded_len_is_the_encoded_length_without_encoding() {
        assert_eq!(
            encoded_len(&slot().ops),
            encode_slot(&slot()).unwrap().len()
        );
        let big = vec![
            Op::Put {
                id: "a".repeat(256),
                fields: vec![Field::text("é".repeat(100_000)), Field::text("")],
            },
            Op::Delete { id: "gone".into() },
        ];
        let probe = Slot {
            seq: 0,
            first_version: 1,
            ops: big,
        };
        assert_eq!(encoded_len(&probe.ops), encode_slot(&probe).unwrap().len());
    }

    #[test]
    fn a_field_far_over_the_old_8_kib_cap_round_trips() {
        let text = "lorem ipsum ".repeat(INLINE_FIELD_MAX / 12);
        let slot = Slot {
            seq: 1,
            first_version: 1,
            ops: vec![Op::Put {
                id: "big".into(),
                fields: vec![Field::text(text)],
            }],
        };
        assert_eq!(decode_slot(&encode_slot(&slot).unwrap()).unwrap(), slot);
    }

    #[test]
    fn text_len_counts_externalised_strings_whole() {
        let ops = vec![
            Op::Put {
                id: "ab".into(),
                fields: vec![
                    Field::text("x".repeat(INLINE_FIELD_MAX + 7)),
                    Field::Int(1),
                    Field::text("yy"),
                ],
            },
            Op::Delete { id: "cde".into() },
        ];
        assert_eq!(text_len(&ops), 2 + INLINE_FIELD_MAX + 7 + 2 + 3);
    }

    #[test]
    fn the_slot_text_cap_is_the_larger_of_the_slot_bound_and_a_document() {
        assert_eq!(slot_text_cap(1000), MAX_SLOT_BYTES);
        assert_eq!(slot_text_cap(MAX_SLOT_BYTES + 1), MAX_SLOT_BYTES + 1);
        assert!(text_fits(5, 5, 10));
        assert!(!text_fits(5, 6, 10));
        assert!(!text_fits(usize::MAX, 1, usize::MAX));
        assert!(text_fits(0, 0, 0));
    }

    #[test]
    fn the_inline_encoder_refuses_a_string_it_would_size_differently() {
        let slot = Slot {
            seq: 1,
            first_version: 1,
            ops: vec![Op::Put {
                id: "big".into(),
                fields: vec![Field::text("a".repeat(INLINE_FIELD_MAX + 1))],
            }],
        };
        assert!(encode_slot(&slot).is_err());
    }

    #[test]
    fn a_large_string_is_replaced_by_a_reference_and_resolved_on_decode() {
        let big = "lorem ipsum ".repeat(INLINE_FIELD_MAX / 6);
        assert!(big.len() > INLINE_FIELD_MAX);
        let slot = Slot {
            seq: 9,
            first_version: 1,
            ops: vec![
                Op::Put {
                    id: "small".into(),
                    fields: vec![Field::text("x")],
                },
                Op::Put {
                    id: "big".into(),
                    fields: vec![Field::Int(1), Field::text(big.as_str())],
                },
            ],
        };
        let mut stored = Vec::new();
        let bytes = encode_slot_with(&slot, &mut |op, field, text| {
            stored.push((op, field, text.to_string()));
            Ok(Some(Blob {
                epoch: 4,
                attempt: 9,
                chunks: 2,
                len: text.len() as u64,
            }))
        })
        .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!((stored[0].0, stored[0].1), (1, 1));
        assert!(
            bytes.len() < 200,
            "the slot holds a reference, not the text"
        );
        assert_eq!(bytes.len(), encoded_len(&slot.ops));
        let decoded = decode_slot_with(&bytes, &mut |at, blob| {
            assert_eq!((at.seq, at.op, at.field), (9, 1, 1));
            assert_eq!((blob.epoch, blob.chunks), (4, 2));
            Ok(stored[0].2.clone())
        })
        .unwrap();
        assert_eq!(decoded, slot);
        // Without a resolver a reference is corrupt, and a resolved length that
        // disagrees with the reference is too.
        assert_eq!(decode_slot(&bytes), Err(Error::Corrupt));
        assert_eq!(
            decode_slot_with(&bytes, &mut |_, _| Ok("short".into())),
            Err(Error::Corrupt)
        );
    }

    #[test]
    fn slot_refuses_bad_tags_counts_and_versions() {
        let mut bytes = encode_slot(&slot()).unwrap();
        bytes[8 + 8 + 8 + 4] = 9; // first op tag
        assert_eq!(decode_slot(&bytes), Err(Error::Corrupt));
        let mut zero_version = encode_slot(&slot()).unwrap();
        zero_version[16..24].copy_from_slice(&0i64.to_le_bytes());
        assert_eq!(decode_slot(&zero_version), Err(Error::Corrupt));
        let mut huge = encode_slot(&slot()).unwrap();
        huge[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_slot(&huge), Err(Error::Corrupt));
        let mut empty = encode_slot(&slot()).unwrap();
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
