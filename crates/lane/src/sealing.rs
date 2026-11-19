//! Slot-payload sealing seam (M06.09): the lane commits sealed payloads without knowing a key.
//!
//! A [`SlotCipher`] replaces the **inline payload** of each `Put` record of a slot with bytes it
//! sealed, and opens them again on replay. Everything the lane itself relies on stays in the clear:
//! the slot envelope, position, commit token, and each record's index, sequence and operation. A
//! fence, a seal record, a delete and the head of the log therefore behave exactly as before, and
//! the commit rule is unchanged: **one conditional create per slot, no extra request of any kind**.
//!
//! What a cipher sees is the slot's [`SlotPosition`], so it can bind each payload to the slot,
//! lane, activation and ordinal it was written for (a copied record fails to open elsewhere).
//!
//! * **Seal** happens once per [`crate::LaneWriter`] commit, before the slot is encoded. The encoded
//!   bytes are what the lane resends if a reply is lost, so a retry of the same commit sends
//!   identical bytes; a *new* commit attempt after a certain refusal seals again, and a cipher must
//!   use a fresh key for it (the nonce must never repeat under one key).
//! * **Open** happens on replay through [`read_log_sealed`], [`crate::LaneWriter::open_sealed`] or
//!   [`crate::LaneWriter::open_with_head_sealed`]; the plain [`crate::read_log`] still shows the
//!   sealed bytes as they are stored.
//! * **Memory** is bounded by the slot: one arena of at most the slot body bound, reused between
//!   commits and dropped above [`RETAIN`] so one large slot does not pin its buffer for life.
//!
//! Not covered: an `External` payload (its own object is sealed by the object decorator; its length
//! and plaintext digest are record fields and stay in the clear) and a record's index, sequence and
//! operation, which the lane needs to admit, order and fence.
use std::ops::Range;
use std::sync::Arc;

use hsearch_format::{
    CommitToken, Layout, MAX_SLOT_BODY_BYTES, PayloadRef, RecordSpec, SlotBody, SlotPosition,
    SlotView, encode_slot,
};
use hsearch_object::MetadataStore;

use crate::log::read_log;
use crate::members::LaneId;
use crate::{Error, Result};

/// A cipher's arena is dropped when it grows past this between commits.
pub const RETAIN: usize = 1 << 20;

/// Why sealing or opening refused. Never carries key material, a payload or provider text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SealFailure {
    /// The key or the caller is not allowed (revoked, destroyed, outside the allowlist).
    Denied,
    /// The key provider could not answer now; nothing was written. Retry later.
    Unavailable,
    /// Stored bytes did not authenticate or are not a sealed payload: damaged, substituted, copied
    /// from another slot or index, sealed under another key, or unsealed in a keyed index.
    Corrupt,
    /// A sealed form or suite this build does not read.
    Unsupported,
    /// The sealed form would not fit the slot's bound.
    TooLarge,
}

/// One arena of sealed or opened payload bytes for a slot, with one optional span per record.
#[derive(Debug, Default)]
pub struct Payloads {
    arena: Vec<u8>,
    spans: Vec<Option<Range<usize>>>,
}

impl Payloads {
    /// Forget the previous slot and expect `records` records, none of them replaced yet.
    pub fn begin(&mut self, records: usize) {
        self.arena.clear();
        self.spans.clear();
        self.spans.resize(records, None);
    }

    /// Reserve `len` zeroed bytes as the replacement payload of record `record` and return them to
    /// fill in place. Refused if the record number is out of range, already replaced, or the arena
    /// would pass the slot body bound.
    pub fn reserve(
        &mut self,
        record: usize,
        len: usize,
    ) -> core::result::Result<&mut [u8], SealFailure> {
        let start = self.arena.len();
        let end = start.checked_add(len).ok_or(SealFailure::TooLarge)?;
        if end > MAX_SLOT_BODY_BYTES {
            return Err(SealFailure::TooLarge);
        }
        match self.spans.get_mut(record) {
            Some(span @ None) => *span = Some(start..end),
            _ => return Err(SealFailure::Corrupt),
        }
        self.arena.resize(end, 0);
        Ok(&mut self.arena[start..end])
    }

    /// The replacement payload of record `record`, if the cipher made one.
    #[must_use]
    pub fn get(&self, record: usize) -> Option<&[u8]> {
        let span = self.spans.get(record)?.as_ref()?;
        self.arena.get(span.clone())
    }

    /// Bytes of capacity held between uses.
    #[must_use]
    pub fn retained(&self) -> usize {
        self.arena.capacity()
    }

    pub(crate) fn trim(&mut self) {
        if self.arena.capacity() > RETAIN {
            self.arena = Vec::new();
        }
    }
}

/// Seals and opens the inline payloads of one slot's records. Synchronous, like the key provider
/// contract it sits on; an async runtime runs a network provider adapter off its executor threads.
pub trait SlotCipher: Send + Sync {
    /// Replace, through `out`, the payload of every record of the slot at `at` that must not be
    /// stored in the clear. A record it leaves alone keeps its payload. Must use a key that no
    /// other call uses (a retry of a refused commit is another call).
    fn seal(
        &self,
        at: &SlotPosition,
        records: &[RecordSpec<'_>],
        out: &mut Payloads,
    ) -> core::result::Result<(), SealFailure>;

    /// The inverse of [`SlotCipher::seal`] for the stored `records` of the slot at `at`: write the
    /// plaintext of every record it sealed to `out`, and refuse the slot if any fails to
    /// authenticate or a record that must be sealed is not.
    fn open(
        &self,
        at: &SlotPosition,
        records: &[RecordSpec<'_>],
        out: &mut Payloads,
    ) -> core::result::Result<(), SealFailure>;
}

impl From<SealFailure> for Error {
    fn from(failure: SealFailure) -> Self {
        Self::Seal(failure)
    }
}

/// Seal `records` with `cipher` and append the encoded data slot to `out`. A refusal leaves `out`
/// as it was. The same bytes are what a baseline that stores a slot as an object (the head-CAS
/// comparison) writes, so both protocols commit identical sealed slots.
pub fn encode_sealed_slot(
    cipher: &dyn SlotCipher,
    out: &mut Vec<u8>,
    payloads: &mut Payloads,
    at: &SlotPosition,
    token: CommitToken,
    records: &[RecordSpec<'_>],
) -> Result<()> {
    payloads.begin(records.len());
    cipher.seal(at, records, payloads)?;
    let sealed: &Payloads = payloads;
    let specs: Vec<RecordSpec<'_>> = records
        .iter()
        .enumerate()
        .map(|(i, record)| match sealed.get(i) {
            Some(bytes) => RecordSpec {
                payload: PayloadRef::Inline(bytes),
                ..*record
            },
            None => *record,
        })
        .collect();
    encode_slot(out, at, token, &SlotBody::Data(&specs))?;
    Ok(())
}

/// The records of `view` with every sealed payload opened into `scratch`. A fence has no records.
pub fn open_records<'s>(
    cipher: &dyn SlotCipher,
    view: &SlotView<'s>,
    scratch: &'s mut Payloads,
) -> Result<Vec<RecordSpec<'s>>> {
    let stored: Vec<RecordSpec<'s>> = view.records().collect();
    if stored.is_empty() {
        return Ok(stored);
    }
    scratch.begin(stored.len());
    cipher.open(&view.position, &stored, scratch)?;
    let opened: &'s Payloads = scratch;
    Ok(stored
        .into_iter()
        .enumerate()
        .map(|(i, record)| match opened.get(i) {
            Some(plain) => RecordSpec {
                payload: PayloadRef::Inline(plain),
                ..record
            },
            None => record,
        })
        .collect())
}

/// [`read_log`] with each slot's sealed payloads opened. `visit` gets the stored view (position,
/// token, kind) and the records with plaintext inline payloads. Memory is one slot at a time.
pub async fn read_log_sealed<S, F>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
    from: u64,
    to: Option<u64>,
    cipher: &dyn SlotCipher,
    mut visit: F,
) -> Result<u64>
where
    S: MetadataStore,
    F: FnMut(&SlotView<'_>, &[RecordSpec<'_>]) -> Result<()>,
{
    let mut scratch = Payloads::default();
    read_log(store, layout, id, from, to, |view| {
        let records = open_records(cipher, view, &mut scratch)?;
        visit(view, &records)
    })
    .await
}

/// What `open_sealed` and `open_with_head_sealed` keep: the cipher the writer seals with.
pub(crate) type Shared = Arc<dyn SlotCipher>;
