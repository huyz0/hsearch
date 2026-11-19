//! The slot-level sealer (M06.09): a fresh data key per (slot, index), wrapped by the index's key and
//! stored inline in the first sealed record of that index, so a commit slot of a keyed index carries
//! no plaintext document.
//!
//! ```text
//! sealed inline payload:  flags u8 | [wrapper] | ciphertext | tag [16]
//! flags:                  bit 0 = suite AES-256-GCM (always set), bit 1 = a wrapper follows,
//!                         bit 2 = the key is shared by every index of its key (`PerKey`)
//! wrapper:                key_ref_len u16 | key_version_len u16 | wrapped_len u16 | key_ref | key_version | wrapped
//! nonce:                  eight zero bytes | record position in the slot, u32 big endian
//! AAD:                    lane record AAD (domain, activation, lane, ordinal, record position, index)
//!                         | seq u64 | flags u8 | digest of every record's clear fields in the slot
//! ```
//!
//! * **Fresh key, never a repeated nonce.** One provider wrap per distinct keyed index in a slot,
//!   made when the slot is sealed. Positions are unique in a slot, so a key sees each nonce once, and
//!   a key is used for one sealing: a commit attempt that was refused and is repeated seals again
//!   under a new key. A lost reply resends the encoded bytes the lane already holds, so a resend is
//!   identical and never a second encryption.
//! * **Bound to where it was written.** The key context the provider authenticates names the index
//!   that carries the wrapper, the crypto domain and an object id derived from the slot position and
//!   that index, so a wrapped key copied to another slot or index will not unwrap. The AAD binds the payload to its slot, record
//!   position, index and sequence, so a sealed record copied, reordered or re-sequenced fails to open.
//! * **Where the key is shared is a setting ([`DekScope`]).** `PerKey` (the default) wraps one data key
//!   per slot for every index that resolves to the same key reference and crypto domain, so a slot of many sparse tenants on the deployment key costs one provider wrap, not one per
//!   tenant. `PerIndex` wraps one per index per slot, which binds each wrapped key to its own index in
//!   the provider's authenticated context. The scope is recorded in the payload's flags, so a lane may
//!   hold slots of both and replay honours each. A [`SlotScopes`] can override the choice for an index.
//! * **First record of a key carries the wrapper.** Later records of that key in the slot carry only a
//!   flag byte and the tag, so a hot tenant pays the wrapper once per slot. Under `PerIndex` a sparse
//!   tenant (one record per slot) pays it for every record: measured at about +11% slot bytes and one
//!   provider wrap per record (`docs/plan/evidence/m06-09-sealed-slots.md`).
//! * **Keyed or not is the scope's decision**, never the bytes': a keyed index's payload that does
//!   not authenticate is refused, never read as plaintext, so a writer cannot downgrade it.
//! * **Revocation applies to replay.** Opening asks the key manager for the data key under the
//!   current policy generation, so a revoked or destroyed key makes the slot unreadable.
//!
//! Not covered: the record's index, sequence and operation (the lane admits, orders and fences on
//! them), and an `External` payload record (its object is sealed by [`crate::SealedBackend`]; its
//! length and plaintext digest are record fields).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aes_gcm::aead::inout::InOutBuf;
use aes_gcm::aead::{AeadInOut, Nonce, Tag};
use aes_gcm::{Aes256Gcm, KeyInit};
use hsearch_format::{Id, PayloadRef, RecordOp, RecordSpec, SlotPosition, aad};
use hsearch_lane::{Payloads, SealFailure, SlotCipher};
use hsearch_security::keys::{CallOptions, KeyContext, KeyRef, KeyVersion, WrappedKey};
use hsearch_security::manager::KeyManager;
use sha2::{Digest, Sha256};

use crate::{Scope, key_error};

const SUITE: u8 = 0x01;
const WRAPPED: u8 = 0x02;
const SHARED: u8 = 0x04;
const KNOWN: u8 = SUITE | WRAPPED | SHARED;
const TAG_LEN: usize = 16;
const WRAPPER_HEAD: usize = 6;
const AAD_LEN: usize = 72 + 8 + 1 + 32;

/// How far one data key per slot reaches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DekScope {
    /// One data key per slot for every index with the same key reference and crypto domain: one provider wrap for a slot of any number of tenants on one key. The wrapped
    /// key's authenticated context names the index that carries it, so the provider does not bind it
    /// to the other indexes of the slot; the payload AAD still binds every record to its own index.
    #[default]
    PerKey,
    /// One data key per index per slot: a wrap for each, each bound to its own index by the provider.
    PerIndex,
}

/// Maps a record's index to its key scope.
pub trait SlotScopes: Send + Sync {
    /// The scope, or `None` for an index with no managed key (its payloads are stored as given).
    /// A failure refuses the whole slot: an unknown index is never given a default scope.
    fn scope(&self, index: &Id) -> Result<Option<Scope>, SealFailure>;

    /// This index's choice of [`DekScope`], or `None` for the sealer's own.
    fn dek_scope(&self, _index: &Id) -> Option<DekScope> {
        None
    }
}

/// What a sealer has done since it was made; for metrics and for asserting the cost.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SlotStats {
    /// Slots sealed (calls to seal that sealed at least one payload).
    pub slots: u64,
    /// Payloads sealed.
    pub records: u64,
    /// Provider wraps: one per data key per slot sealed.
    pub wraps: u64,
    /// Plaintext bytes sealed.
    pub plain_bytes: u64,
    /// Bytes they became (ciphertext, tags, flags and wrappers).
    pub sealed_bytes: u64,
    /// Payloads opened.
    pub opened: u64,
}

#[derive(Default)]
struct Counters {
    slots: AtomicU64,
    records: AtomicU64,
    wraps: AtomicU64,
    plain: AtomicU64,
    sealed: AtomicU64,
    opened: AtomicU64,
}

/// The slot-level [`SlotCipher`]. See the module documentation.
pub struct SlotSealer {
    keys: Arc<KeyManager>,
    scopes: Arc<dyn SlotScopes>,
    dek_scope: DekScope,
    counters: Counters,
}

impl SlotSealer {
    /// A sealer over `keys`, resolving each record's index through `scopes`.
    #[must_use]
    pub fn new(keys: Arc<KeyManager>, scopes: Arc<dyn SlotScopes>) -> Self {
        Self {
            keys,
            scopes,
            dek_scope: DekScope::default(),
            counters: Counters::default(),
        }
    }

    /// The sealer's choice of [`DekScope`] for an index that makes none. Replay honours what each
    /// payload recorded, whatever this is now.
    #[must_use]
    pub fn with_dek_scope(mut self, scope: DekScope) -> Self {
        self.dek_scope = scope;
        self
    }

    /// Counters since the sealer was made.
    #[must_use]
    pub fn stats(&self) -> SlotStats {
        let c = &self.counters;
        SlotStats {
            slots: c.slots.load(Ordering::Relaxed),
            records: c.records.load(Ordering::Relaxed),
            wraps: c.wraps.load(Ordering::Relaxed),
            plain_bytes: c.plain.load(Ordering::Relaxed),
            sealed_bytes: c.sealed.load(Ordering::Relaxed),
            opened: c.opened.load(Ordering::Relaxed),
        }
    }

    /// The most a sealed payload can exceed its plaintext by, for a wrapper of `wrapper` bytes:
    /// flags, tag and, on the first record of an index in a slot, the wrapper itself.
    #[must_use]
    pub const fn overhead(first_of_index: bool, wrapper: usize) -> usize {
        1 + TAG_LEN
            + if first_of_index {
                WRAPPER_HEAD + wrapper
            } else {
                0
            }
    }
}

/// The object id the provider binds a slot's key to: derived from the position and the index, so a
/// wrapped key presented for another slot, lane, activation or index fails to unwrap.
fn slot_object(at: &SlotPosition, index: &Id) -> [u8; 16] {
    let mut h = Sha256::new();
    h.update(b"hs-slot-dek\x00\x01");
    h.update(at.domain);
    h.update(at.activation.to_le_bytes());
    h.update(at.lane.to_le_bytes());
    h.update(at.ordinal.to_le_bytes());
    h.update(index);
    let digest = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn context(scope: &Scope, at: &SlotPosition, index: &Id) -> KeyContext {
    KeyContext {
        index: scope.index.clone(),
        domain: scope.domain,
        object: slot_object(at, index),
    }
}

fn nonce_of(record: usize) -> Result<Nonce<Aes256Gcm>, SealFailure> {
    let position = u32::try_from(record).map_err(|_| SealFailure::TooLarge)?;
    let mut n = [0u8; 12];
    n[8..].copy_from_slice(&position.to_be_bytes());
    Ok(n.into())
}

fn aad_of(
    at: &SlotPosition,
    record: usize,
    spec: &RecordSpec<'_>,
    flags: u8,
    headers: &[u8; 32],
) -> Result<[u8; AAD_LEN], SealFailure> {
    let position = u32::try_from(record).map_err(|_| SealFailure::TooLarge)?;
    let base = aad(
        &at.domain,
        at.activation,
        at.lane,
        at.ordinal,
        position,
        &spec.index,
    );
    let mut out = [0u8; AAD_LEN];
    out[..72].copy_from_slice(&base);
    out[72..80].copy_from_slice(&spec.seq.to_le_bytes());
    out[80] = flags;
    out[81..].copy_from_slice(headers);
    Ok(out)
}

/// A digest of every record's clear fields (count, index, sequence, operation, payload kind and an
/// external record's location), bound into each sealed payload's AAD: a record turned into another
/// operation, or dropped from the tail, changes it and every sealed payload of the slot stops opening.
fn headers_digest(records: &[RecordSpec<'_>]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"hs-slot-headers\x00\x01");
    h.update((records.len() as u32).to_le_bytes());
    for r in records {
        h.update(r.index);
        h.update(r.seq.to_le_bytes());
        h.update([r.op as u8]);
        match r.payload {
            PayloadRef::None => h.update([0u8]),
            PayloadRef::Inline(_) => h.update([1u8]),
            PayloadRef::External {
                object,
                length,
                digest,
            } => {
                h.update([2u8]);
                h.update(object);
                h.update(length.to_le_bytes());
                h.update(digest);
            }
        }
    }
    h.finalize().into()
}

fn failure(error: hsearch_object::Error) -> SealFailure {
    match error {
        hsearch_object::Error::PermissionDenied => SealFailure::Denied,
        hsearch_object::Error::Throttled
        | hsearch_object::Error::Unavailable
        | hsearch_object::Error::Cancelled => SealFailure::Unavailable,
        hsearch_object::Error::Unsupported => SealFailure::Unsupported,
        _ => SealFailure::Corrupt,
    }
}

/// Which records of a slot share one data key: all of a key (`PerKey`) or one index (`PerIndex`).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Group {
    key: KeyRef,
    domain: [u8; 16],
    index: Option<Id>,
}

/// An index's resolved scope and key group within one slot.
struct Member {
    scope: Scope,
    group: Group,
    shared: bool,
}

impl SlotSealer {
    /// The key group of `index`, or `None` if it has no managed key.
    fn member(&self, index: &Id) -> Result<Option<Member>, SealFailure> {
        let Some(scope) = self.scopes.scope(index)? else {
            return Ok(None);
        };
        let shared = self.scopes.dek_scope(index).unwrap_or(self.dek_scope) == DekScope::PerKey;
        let group = Group {
            key: self.keys.resolve(scope.key.as_ref()).clone(),
            domain: scope.domain,
            index: (!shared).then_some(*index),
        };
        Ok(Some(Member {
            scope,
            group,
            shared,
        }))
    }
}

/// A data key within one slot being sealed.
struct Sealing {
    cipher: Aes256Gcm,
    wrapped: WrappedKey,
    wrapper_pending: bool,
}

impl SlotCipher for SlotSealer {
    fn seal(
        &self,
        at: &SlotPosition,
        records: &[RecordSpec<'_>],
        out: &mut Payloads,
    ) -> Result<(), SealFailure> {
        // `None` is an index with no managed key: its payload stays as given.
        let headers = headers_digest(records);
        let mut members: HashMap<Id, Option<Member>> = HashMap::new();
        let mut keys: HashMap<Group, Sealing> = HashMap::new();
        let (mut sealed_records, mut wraps, mut plain_bytes, mut sealed_bytes) =
            (0u64, 0u64, 0u64, 0u64);
        for (i, record) in records.iter().enumerate() {
            let (RecordOp::Put, PayloadRef::Inline(plain)) = (record.op, record.payload) else {
                continue;
            };
            if let Entry::Vacant(slot) = members.entry(record.index) {
                slot.insert(self.member(&record.index)?);
            }
            let Some(Some(member)) = members.get(&record.index) else {
                continue;
            };
            if !keys.contains_key(&member.group) {
                // The first record of a group carries its wrapper, and the provider binds the key to
                // that record's index and slot position.
                let ctx = context(&member.scope, at, &record.index);
                let (dek, wrapped) = self
                    .keys
                    .create_object_key(
                        member.scope.key.as_ref(),
                        &ctx,
                        member.scope.generation,
                        &CallOptions::default(),
                    )
                    .map_err(|e| failure(key_error(e)))?;
                wraps += 1;
                let cipher =
                    Aes256Gcm::new_from_slice(dek.expose()).map_err(|_| SealFailure::Corrupt)?;
                keys.insert(
                    member.group.clone(),
                    Sealing {
                        cipher,
                        wrapped,
                        wrapper_pending: true,
                    },
                );
            }
            let Some(state) = keys.get_mut(&member.group) else {
                continue;
            };
            let key = state.wrapped.key().as_str().as_bytes();
            let version = state.wrapped.version().as_str().as_bytes();
            let wrap = state.wrapped.bytes();
            let wrapper = if state.wrapper_pending {
                WRAPPER_HEAD + key.len() + version.len() + wrap.len()
            } else {
                0
            };
            let flags = SUITE
                | if state.wrapper_pending { WRAPPED } else { 0 }
                | if member.shared { SHARED } else { 0 };
            let total = 1 + wrapper + plain.len() + TAG_LEN;
            let buffer = out.reserve(i, total)?;
            buffer[0] = flags;
            let mut at_byte = 1;
            if state.wrapper_pending {
                // The wrapper's parts were bounded when the wrapped key was built, so these cannot truncate.
                for len in [key.len(), version.len(), wrap.len()] {
                    let len = u16::try_from(len).map_err(|_| SealFailure::TooLarge)?;
                    buffer[at_byte..at_byte + 2].copy_from_slice(&len.to_le_bytes());
                    at_byte += 2;
                }
                for part in [key, version, wrap] {
                    buffer[at_byte..at_byte + part.len()].copy_from_slice(part);
                    at_byte += part.len();
                }
                state.wrapper_pending = false;
            }
            let (ciphertext, tag_out) = buffer[at_byte..].split_at_mut(plain.len());
            let tag = state
                .cipher
                .encrypt_inout_detached(
                    &nonce_of(i)?,
                    &aad_of(at, i, record, flags, &headers)?,
                    InOutBuf::new(plain, ciphertext).map_err(|_| SealFailure::Corrupt)?,
                )
                .map_err(|_| SealFailure::Corrupt)?;
            tag_out.copy_from_slice(&tag);
            sealed_records += 1;
            plain_bytes += plain.len() as u64;
            sealed_bytes += total as u64;
        }
        if sealed_records > 0 {
            let c = &self.counters;
            c.slots.fetch_add(1, Ordering::Relaxed);
            c.records.fetch_add(sealed_records, Ordering::Relaxed);
            c.wraps.fetch_add(wraps, Ordering::Relaxed);
            c.plain.fetch_add(plain_bytes, Ordering::Relaxed);
            c.sealed.fetch_add(sealed_bytes, Ordering::Relaxed);
        }
        Ok(())
    }

    fn open(
        &self,
        at: &SlotPosition,
        records: &[RecordSpec<'_>],
        out: &mut Payloads,
    ) -> Result<(), SealFailure> {
        let headers = headers_digest(records);
        let mut members: HashMap<Id, Option<Member>> = HashMap::new();
        // The keys this slot's wrappers have opened: one per index, and one per key group.
        let mut own: HashMap<Id, Aes256Gcm> = HashMap::new();
        let mut shared: HashMap<Group, Aes256Gcm> = HashMap::new();
        let mut opened = 0u64;
        for (i, record) in records.iter().enumerate() {
            let (RecordOp::Put, PayloadRef::Inline(stored)) = (record.op, record.payload) else {
                continue;
            };
            if let Entry::Vacant(slot) = members.entry(record.index) {
                slot.insert(self.member(&record.index)?);
            }
            let Some(Some(member)) = members.get(&record.index) else {
                continue;
            };
            let (&flags, rest) = stored.split_first().ok_or(SealFailure::Corrupt)?;
            if flags & SUITE == 0 || flags & !KNOWN != 0 {
                // A keyed index's payload that is not a sealed payload of this build is never plaintext.
                return Err(SealFailure::Corrupt);
            }
            // What the payload recorded decides how its key is found, not what the sealer does now.
            let group = Group {
                index: (flags & SHARED == 0).then_some(record.index),
                ..member.group.clone()
            };
            let known = |own: &HashMap<Id, Aes256Gcm>, shared: &HashMap<Group, Aes256Gcm>| {
                if flags & SHARED == 0 {
                    own.contains_key(&record.index)
                } else {
                    shared.contains_key(&group)
                }
            };
            let body = if flags & WRAPPED != 0 {
                if known(&own, &shared) {
                    // The sealer writes a key's wrapper once per slot; a second would swap the key mid-slot.
                    return Err(SealFailure::Corrupt);
                }
                let (wrapped, body) = parse_wrapper(rest)?;
                let ctx = context(&member.scope, at, &record.index);
                let dek = self
                    .keys
                    .open_object_key(
                        &wrapped,
                        &ctx,
                        member.scope.generation,
                        &CallOptions::default(),
                    )
                    .map_err(|e| failure(key_error(e)))?;
                let cipher =
                    Aes256Gcm::new_from_slice(dek.expose()).map_err(|_| SealFailure::Corrupt)?;
                if flags & SHARED == 0 {
                    own.insert(record.index, cipher);
                } else {
                    shared.insert(group.clone(), cipher);
                }
                body
            } else {
                rest
            };
            let cipher = if flags & SHARED == 0 {
                own.get(&record.index)
            } else {
                shared.get(&group)
            }
            .ok_or(SealFailure::Corrupt)?;
            let plain_len = body
                .len()
                .checked_sub(TAG_LEN)
                .filter(|n| *n > 0)
                .ok_or(SealFailure::Corrupt)?;
            let (ciphertext, tag) = body.split_at(plain_len);
            let tag = Tag::<Aes256Gcm>::try_from(tag).map_err(|_| SealFailure::Corrupt)?;
            let buffer = out.reserve(i, plain_len)?;
            cipher
                .decrypt_inout_detached(
                    &nonce_of(i)?,
                    &aad_of(at, i, record, flags, &headers)?,
                    InOutBuf::new(ciphertext, buffer).map_err(|_| SealFailure::Corrupt)?,
                    &tag,
                )
                .map_err(|_| SealFailure::Corrupt)?;
            opened += 1;
        }
        self.counters.opened.fetch_add(opened, Ordering::Relaxed);
        Ok(())
    }
}

fn parse_wrapper(bytes: &[u8]) -> Result<(WrappedKey, &[u8]), SealFailure> {
    let head = bytes.get(..WRAPPER_HEAD).ok_or(SealFailure::Corrupt)?;
    let len = |at: usize| usize::from(u16::from_le_bytes([head[at], head[at + 1]]));
    let (key, version, wrap) = (len(0), len(2), len(4));
    let end = WRAPPER_HEAD + key + version + wrap;
    let all = bytes.get(WRAPPER_HEAD..end).ok_or(SealFailure::Corrupt)?;
    let (key_bytes, all) = all.split_at(key);
    let (version_bytes, wrap_bytes) = all.split_at(version);
    let key_ref = KeyRef::new(std::str::from_utf8(key_bytes).map_err(|_| SealFailure::Corrupt)?)
        .map_err(|_| SealFailure::Corrupt)?;
    let key_version =
        KeyVersion::new(std::str::from_utf8(version_bytes).map_err(|_| SealFailure::Corrupt)?)
            .map_err(|_| SealFailure::Corrupt)?;
    let wrapped = WrappedKey::new(key_ref, key_version, wrap_bytes.to_vec())
        .map_err(|_| SealFailure::Corrupt)?;
    Ok((wrapped, &bytes[end..]))
}
