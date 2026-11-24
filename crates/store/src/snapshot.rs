//! Shallow snapshots and same-store restore (PR-50): metadata-only records that hold an index's
//! manifest roots alive with durable pins, and a restore that copies nothing.
//!
//! A snapshot names, per shard, the manifest root the shard's head names at that moment, and holds
//! it with a pin that never expires on its own (`u64::MAX`) and is released only by deleting the
//! snapshot. Pins are registered **before** the record is written, so a snapshot that exists is
//! always held. The pin, the record and a restored index's origin record are the only objects a
//! snapshot or restore writes: no page, file or root is read or copied, so the request count depends
//! on the number of shards (at most [`MAX_SHARDS`]) and on nothing else.
//!
//! A restored index is a record that says "the objects of snapshot S of index I": reads resolve a
//! file through the source index's payload key ([`Snapshots::files`]). It holds nothing of its own
//! in the sweep's eyes. Instead it is registered as a **holder** on the snapshot record (a
//! compare-and-swap, at most [`MAX_HOLDERS`] of them), and a snapshot with holders that is deleted
//! becomes `Retired`: unusable for new restores but keeping its pins until the last holder is
//! dropped ([`Snapshots::drop_restore`]). A new pin could not do this job: the boundary refuses a
//! pin on a generation a sweep has already passed, and a snapshot's base is below it by design.
//!
//! Deleting (no holders) marks the record, releases its pins, then removes the record, so a delete
//! stopped anywhere leaves a record that is refused for restore and that the next delete finishes;
//! a record never exists with its pins gone.
//!
//! One creator per snapshot id: the catalog's claim of the snapshot name is what makes the id
//! unique. A create that fails after registering pins releases them; one stopped by a crash leaves
//! them until the same create is retried (it adopts pins it finds with the same id) and then
//! deleted.
use std::sync::Arc;
use std::time::Duration;

use hsearch_format::{
    CommitToken, Digest, FileEntry, FormatError, Id, Layout, ManifestRoot, PinRecord,
};
use hsearch_lane::{block_on, read_head};
use hsearch_object::{Error as StoreError, MetadataStore, Precondition};
use sha2::{Digest as _, Sha256};

use crate::Requests;
use crate::io::{Io, request_id};
use crate::pins::{self, Pins};
use crate::pointer;
use crate::sweep::ShardRef;

/// Most shards one snapshot record names. A record is at most about 0.5 MiB at this bound; an
/// index with more shards needs a paged record, which this form does not have.
pub const MAX_SHARDS: usize = 4096;

/// Most restored indexes that may share one snapshot at once.
pub const MAX_HOLDERS: usize = 64;

/// How many times a create re-reads the head after a sweep took the base it was about to pin.
const CREATE_ATTEMPTS: usize = 3;

const MAGIC: [u8; 4] = *b"HSSN";
const VERSION: u8 = 1;
const KIND_SNAPSHOT: u8 = 1;
const KIND_ORIGIN: u8 = 2;
const SHARD_BYTES: usize = 16 + 8 + 1 + (16 + 8 + 8 + 32 + 16 + 16);
const HEAD_BYTES: usize = 4 + 1 + 1 + 1 + 16 + 16 + 16 + 8 + 4;
const HOLDERS_BYTES: usize = 2 + MAX_HOLDERS * 16;
const CHECK_BYTES: usize = 16;
const MAX_RECORD: usize = HEAD_BYTES + MAX_SHARDS * SHARD_BYTES + HOLDERS_BYTES + CHECK_BYTES;

/// Whether the key envelope an object depends on may still be used.
pub trait Keys {
    /// False when the key behind `envelope` is revoked or destroyed.
    fn usable(&self, envelope: &Id) -> bool;
}

/// A key view that accepts every envelope (for stores with no keys to revoke).
pub struct AnyKeys;
impl Keys for AnyKeys {
    fn usable(&self, _: &Id) -> bool {
        true
    }
}

/// The root of one shard a snapshot or restore holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootRef {
    /// The manifest root held.
    pub manifest: Id,
    /// Its generation.
    pub generation: u64,
    /// Slots below this ordinal are contained in its files.
    pub covered: u64,
    /// Mapping the files were written under (a snapshot keeps the one it had).
    pub mapping: Digest,
    /// The key envelope the files depend on.
    pub crypto_envelope: Id,
    /// The pin that holds it.
    pub pin: Id,
}

/// One shard incarnation a record names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShardPin {
    /// The shard.
    pub shard: Id,
    /// Its incarnation.
    pub incarnation: u64,
    /// The root held, or `None` for a shard with no base yet.
    pub root: Option<RootRef>,
}

/// Whether a snapshot may be restored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    /// Usable.
    Active,
    /// Being deleted: its pins are being released, so it is not usable.
    Deleting,
    /// Deleted while restored indexes still share its objects: not usable for new restores, pins
    /// kept until the last holder is dropped.
    Retired,
}

/// A snapshot record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotRecord {
    /// The snapshot's id.
    pub snapshot: Id,
    /// The index snapshotted.
    pub index: Id,
    /// Whether it may be restored.
    pub state: State,
    /// Creation time, Unix milliseconds.
    pub created_unix_ms: u64,
    /// One entry per shard.
    pub shards: Vec<ShardPin>,
    /// The restored indexes sharing this snapshot's objects.
    pub holders: Vec<Id>,
}

/// The record a restored index keeps naming the objects it shares.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OriginRecord {
    /// The restored index.
    pub index: Id,
    /// The index whose objects it shares: the one that is attributed their storage until detached.
    pub source: Id,
    /// The snapshot it was restored from.
    pub snapshot: Id,
    /// Creation time, Unix milliseconds.
    pub created_unix_ms: u64,
    /// One entry per shard (the snapshot's, whose pins the restore shares).
    pub shards: Vec<ShardPin>,
}

/// Why an operation failed.
#[derive(Debug)]
pub enum Error {
    /// No such record.
    NotFound,
    /// A record with that id exists.
    AlreadyExists,
    /// The snapshot is being deleted; run the delete again to finish it.
    Deleting,
    /// The snapshot is shared by [`MAX_HOLDERS`] restored indexes already.
    TooManyHolders,
    /// Another writer changed the record at the same time; run it again.
    Contended,
    /// A snapshot must name at least one shard.
    NoShards,
    /// More shards than [`MAX_SHARDS`].
    TooManyShards,
    /// The key a root depends on is revoked: nothing was written or read.
    KeyUnavailable,
    /// A stored record is not valid, or names something that is not there.
    Corrupt(&'static str),
    /// A structural head could not be read.
    Head(String),
    /// A pin step failed.
    Pin(pins::Error),
    /// A stored object failed its format checks.
    Format(FormatError),
    /// The store failed.
    Store(StoreError),
}

impl From<pointer::Error> for Error {
    fn from(e: pointer::Error) -> Self {
        match e {
            pointer::Error::Store(e) => Self::Store(e),
            pointer::Error::Corrupt(e) => Self::Format(e),
            pointer::Error::Refused(_) | pointer::Error::Lost => Self::Corrupt("base pointer"),
        }
    }
}
impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
impl From<FormatError> for Error {
    fn from(e: FormatError) -> Self {
        Self::Format(e)
    }
}
impl From<pins::Error> for Error {
    fn from(e: pins::Error) -> Self {
        Self::Pin(e)
    }
}

/// What a create did.
#[derive(Debug)]
pub struct Created {
    /// The record written.
    pub record: SnapshotRecord,
    /// Requests made here and through the pins (not the structural-head reads).
    pub requests: Requests,
    /// Structural-head reads made; each is one HEAD and one ranged read.
    pub head_reads: u64,
}

/// What a restore did.
#[derive(Debug)]
pub struct Restored {
    /// The origin record written.
    pub record: OriginRecord,
    /// Requests made.
    pub requests: Requests,
}

/// What a delete or a dropped restore did.
#[derive(Debug)]
pub struct Released {
    /// Pins released.
    pub pins: usize,
    /// Requests made.
    pub requests: Requests,
}

fn sum(a: Requests, b: Requests) -> Requests {
    Requests {
        reads: a.reads + b.reads,
        writes: a.writes + b.writes,
        deletes: a.deletes + b.deletes,
        lists: a.lists + b.lists,
    }
}

/// A 16-byte identity that is a pure function of its inputs, so a retry names the same pin.
fn derive(domain: &str, parts: &[&[u8]]) -> [u8; 16] {
    let mut h = Sha256::new();
    h.update(b"hs-snapshot\x01");
    h.update(domain.as_bytes());
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    let d = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(d.get(..16).unwrap_or_default());
    out
}

// ---- records -------------------------------------------------------------------------------

fn put_shards(out: &mut Vec<u8>, shards: &[ShardPin]) {
    for s in shards {
        out.extend_from_slice(&s.shard);
        out.extend_from_slice(&s.incarnation.to_le_bytes());
        match &s.root {
            None => {
                out.push(0);
                out.resize(out.len() + (SHARD_BYTES - 16 - 8 - 1), 0);
            }
            Some(r) => {
                out.push(1);
                out.extend_from_slice(&r.manifest);
                out.extend_from_slice(&r.generation.to_le_bytes());
                out.extend_from_slice(&r.covered.to_le_bytes());
                out.extend_from_slice(&r.mapping);
                out.extend_from_slice(&r.crypto_envelope);
                out.extend_from_slice(&r.pin);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn encode(
    kind: u8,
    state: u8,
    owner: &Id,
    source: &Id,
    via: &Id,
    created: u64,
    shards: &[ShardPin],
    holders: &[Id],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEAD_BYTES + shards.len() * SHARD_BYTES + CHECK_BYTES);
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(kind);
    out.push(state);
    out.extend_from_slice(owner);
    out.extend_from_slice(source);
    out.extend_from_slice(via);
    out.extend_from_slice(&created.to_le_bytes());
    out.extend_from_slice(&(shards.len() as u32).to_le_bytes());
    put_shards(&mut out, shards);
    out.extend_from_slice(&(holders.len() as u16).to_le_bytes());
    for h in holders {
        out.extend_from_slice(h);
    }
    let check = Sha256::digest(&out);
    out.extend_from_slice(check.get(..CHECK_BYTES).unwrap_or_default());
    out
}

struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let (head, tail) = self
            .0
            .split_at_checked(n)
            .ok_or(Error::Corrupt("record truncated"))?;
        self.0 = tail;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.take(N)?
            .try_into()
            .map_err(|_| Error::Corrupt("record truncated"))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

struct Decoded {
    kind: u8,
    state: u8,
    owner: Id,
    source: Id,
    via: Id,
    created: u64,
    shards: Vec<ShardPin>,
    holders: Vec<Id>,
}

fn decode(bytes: &[u8]) -> Result<Decoded, Error> {
    if bytes.len() > MAX_RECORD {
        return Err(Error::Corrupt("record too large"));
    }
    let (body, check) = bytes
        .split_at_checked(bytes.len().saturating_sub(CHECK_BYTES))
        .ok_or(Error::Corrupt("record truncated"))?;
    if Sha256::digest(body).get(..CHECK_BYTES) != Some(check) {
        return Err(Error::Corrupt("record checksum"));
    }
    let mut c = Cursor(body);
    if c.take(4)? != MAGIC || c.array::<1>()?[0] != VERSION {
        return Err(Error::Corrupt("record magic or version"));
    }
    let kind = c.array::<1>()?[0];
    let state = c.array::<1>()?[0];
    let owner = c.array()?;
    let source = c.array()?;
    let via = c.array()?;
    let created = c.u64()?;
    let count = u32::from_le_bytes(c.array()?) as usize;
    if count > MAX_SHARDS || count.checked_mul(SHARD_BYTES).is_none_or(|n| n > c.0.len()) {
        return Err(Error::Corrupt("record shard count"));
    }
    let mut shards = Vec::with_capacity(count);
    for _ in 0..count {
        let shard = c.array()?;
        let incarnation = c.u64()?;
        let flag = c.array::<1>()?[0];
        let rest: [u8; SHARD_BYTES - 25] = c.array()?;
        let root = match flag {
            0 => None,
            1 => {
                let mut r = Cursor(&rest);
                Some(RootRef {
                    manifest: r.array()?,
                    generation: r.u64()?,
                    covered: r.u64()?,
                    mapping: r.array()?,
                    crypto_envelope: r.array()?,
                    pin: r.array()?,
                })
            }
            _ => return Err(Error::Corrupt("record shard flag")),
        };
        shards.push(ShardPin {
            shard,
            incarnation,
            root,
        });
    }
    let held = usize::from(u16::from_le_bytes(c.array()?));
    if held > MAX_HOLDERS || held.checked_mul(16) != Some(c.0.len()) {
        return Err(Error::Corrupt("record holder count"));
    }
    let mut holders = Vec::with_capacity(held);
    for _ in 0..held {
        holders.push(c.array()?);
    }
    Ok(Decoded {
        kind,
        state,
        owner,
        source,
        via,
        created,
        shards,
        holders,
    })
}

impl SnapshotRecord {
    fn encode(&self) -> Vec<u8> {
        let state = match self.state {
            State::Active => 1,
            State::Deleting => 2,
            State::Retired => 3,
        };
        encode(
            KIND_SNAPSHOT,
            state,
            &self.snapshot,
            &self.index,
            &[0; 16],
            self.created_unix_ms,
            &self.shards,
            &self.holders,
        )
    }

    /// Decode, requiring the record to be snapshot `snapshot` of `index`.
    fn decode_for(bytes: &[u8], index: &Id, snapshot: &Id) -> Result<Self, Error> {
        let d = decode(bytes)?;
        if d.kind != KIND_SNAPSHOT || d.owner != *snapshot || d.source != *index {
            return Err(Error::Corrupt("record names another snapshot"));
        }
        let state = match d.state {
            1 => State::Active,
            2 => State::Deleting,
            3 => State::Retired,
            _ => return Err(Error::Corrupt("record state")),
        };
        Ok(Self {
            snapshot: *snapshot,
            index: *index,
            state,
            created_unix_ms: d.created,
            shards: d.shards,
            holders: d.holders,
        })
    }
}

impl OriginRecord {
    fn encode(&self) -> Vec<u8> {
        encode(
            KIND_ORIGIN,
            1,
            &self.index,
            &self.source,
            &self.snapshot,
            self.created_unix_ms,
            &self.shards,
            &[],
        )
    }

    fn decode_for(bytes: &[u8], index: &Id) -> Result<Self, Error> {
        let d = decode(bytes)?;
        if d.kind != KIND_ORIGIN || d.owner != *index {
            return Err(Error::Corrupt("record names another index"));
        }
        Ok(Self {
            index: *index,
            source: d.source,
            snapshot: d.via,
            created_unix_ms: d.created,
            shards: d.shards,
        })
    }
}

// ---- the operations ------------------------------------------------------------------------

/// Snapshots and restores of one deployment.
pub struct Snapshots<S> {
    io: Io<S>,
    pins: Pins<S>,
    layout: Layout,
}

/// Which pin a shard of a record gets: a pure function of the owner and the shard.
fn pin_record(
    owner: &Id,
    source: &Id,
    shard: &Id,
    root: &ManifestRoot,
    manifest: &Id,
) -> Result<PinRecord, Error> {
    let token = CommitToken::new(derive("snapshot", &[b"token", owner, shard, manifest]))?;
    Ok(PinRecord {
        pin: derive("snapshot", &[b"pin", owner, shard]),
        index: *source,
        manifest: *manifest,
        manifest_generation: root.generation,
        // A snapshot's hold ends when the snapshot is deleted, never by the clock.
        expires_unix_ms: u64::MAX,
        token,
    })
}

impl<S: MetadataStore> Snapshots<S> {
    /// Snapshots stored through `store`.
    #[must_use]
    pub fn new(store: Arc<S>, layout: Layout, deadline: Duration) -> Self {
        Self {
            io: Io::new(Arc::clone(&store), deadline),
            pins: Pins::new(store, layout.clone(), deadline),
            layout,
        }
    }

    fn requests(&self) -> Requests {
        sum(self.io.counters.snapshot(), self.pins.requests())
    }

    /// The root of `index`'s shard `s`, read from its head, with the root it names.
    fn live_root(&self, index: &Id, s: &ShardRef) -> Result<Option<(Id, ManifestRoot)>, Error> {
        let (head, _) = block_on(read_head(&*self.io.store, &self.layout, &s.lane))
            .map_err(|e| Error::Head(format!("{e:?}")))?;
        let Some((manifest, _)) = pointer::named(
            &self.io,
            &self.layout,
            s.base,
            &head,
            index,
            &s.shard,
            s.incarnation,
        )?
        else {
            return Ok(None);
        };
        let key = self
            .layout
            .manifest_root(index, &s.shard, s.incarnation, &manifest);
        let Some((bytes, _)) = self.io.read(&key)? else {
            // A sweep took the base between the head and the root: the head has moved on.
            return Err(Error::NotFound);
        };
        Ok(Some((manifest, ManifestRoot::decode_for(&bytes, index)?)))
    }

    /// Pin one shard's live root for `owner`, reading the head again if a sweep took the base.
    fn pin_shard(
        &self,
        owner: &Id,
        index: &Id,
        s: &ShardRef,
        keys: &dyn Keys,
        now: u64,
        heads: &mut u64,
    ) -> Result<ShardPin, Error> {
        let mut last = Error::NotFound;
        for _ in 0..CREATE_ATTEMPTS {
            *heads += 1;
            let (manifest, root) = match self.live_root(index, s) {
                Ok(Some(found)) => found,
                Ok(None) => {
                    return Ok(ShardPin {
                        shard: s.shard,
                        incarnation: s.incarnation,
                        root: None,
                    });
                }
                Err(Error::NotFound) => continue,
                Err(e) => return Err(e),
            };
            if !keys.usable(&root.crypto_envelope) {
                return Err(Error::KeyUnavailable);
            }
            let record = pin_record(owner, index, &s.shard, &root, &manifest)?;
            match self
                .pins
                .register_verified(&record, &s.shard, s.incarnation, &root, now)
            {
                Ok(_) => {}
                Err(pins::Error::Store(StoreError::Conflict)) => {
                    // A pin of this id from an unfinished earlier attempt names another root:
                    // there is one creator per id, so replace it.
                    self.pins.release(index, &record.pin)?;
                    self.pins
                        .register_verified(&record, &s.shard, s.incarnation, &root, now)?;
                }
                Err(pins::Error::Swept { boundary }) => {
                    last = Error::Pin(pins::Error::Swept { boundary });
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            return Ok(ShardPin {
                shard: s.shard,
                incarnation: s.incarnation,
                root: Some(RootRef {
                    manifest,
                    generation: root.generation,
                    covered: root.covered,
                    mapping: root.mapping,
                    crypto_envelope: root.crypto_envelope,
                    pin: record.pin,
                }),
            });
        }
        Err(last)
    }

    fn release_all(&self, index: &Id, shards: &[ShardPin]) -> Result<usize, Error> {
        let mut released = 0;
        for s in shards {
            if let Some(r) = &s.root {
                self.pins.release(index, &r.pin)?;
                released += 1;
            }
        }
        Ok(released)
    }

    /// Snapshot `index`: hold the base each of `shards` names now, then write the record.
    ///
    /// Each shard is read at its own moment; the snapshot is per-shard consistent, as the shards'
    /// commits are independent. The cost is, per shard, one head read, one root read, one pin
    /// write and one boundary read; plus one existence check and one write for the record.
    pub fn create(
        &self,
        index: &Id,
        snapshot: &Id,
        shards: &[ShardRef],
        keys: &dyn Keys,
        now_unix_ms: u64,
    ) -> Result<Created, Error> {
        if shards.is_empty() {
            return Err(Error::NoShards);
        }
        if shards.len() > MAX_SHARDS {
            return Err(Error::TooManyShards);
        }
        let key = self.layout.snapshot(index, snapshot);
        if self.io.exists(&key)? {
            return Err(Error::AlreadyExists);
        }
        let mut held: Vec<ShardPin> = Vec::with_capacity(shards.len());
        let mut heads = 0u64;
        let outcome = (|| {
            for s in shards {
                held.push(self.pin_shard(snapshot, index, s, keys, now_unix_ms, &mut heads)?);
            }
            let record = SnapshotRecord {
                snapshot: *snapshot,
                index: *index,
                state: State::Active,
                created_unix_ms: now_unix_ms,
                shards: held.clone(),
                holders: Vec::new(),
            };
            let identity = request_id(&[b"snapshot", key.as_bytes()])?;
            match self
                .io
                .write(&key, &record.encode(), Precondition::Absent, identity)
            {
                Ok(()) => Ok(record),
                Err(StoreError::Conflict) => Err(Error::AlreadyExists),
                Err(e) => Err(e.into()),
            }
        })();
        match outcome {
            Ok(record) => Ok(Created {
                record,
                requests: self.requests(),
                head_reads: heads,
            }),
            // A refused create must not leave pins holding storage for a record that does not exist.
            // (An id that already exists keeps its own: they are not these.)
            Err(Error::AlreadyExists) => Err(Error::AlreadyExists),
            Err(e) => {
                // Also the pin of the shard that failed mid-registration, which `held` lacks: its
                // id is a function of the snapshot and the shard.
                for s in shards {
                    let pin = derive("snapshot", &[b"pin", snapshot, &s.shard]);
                    let _ = self.pins.release(index, &pin);
                }
                Err(e)
            }
        }
    }

    /// Read a snapshot record, or `None` when there is none.
    pub fn read(&self, index: &Id, snapshot: &Id) -> Result<Option<SnapshotRecord>, Error> {
        let key = self.layout.snapshot(index, snapshot);
        match self.io.read(&key)? {
            None => Ok(None),
            Some((bytes, _)) => Ok(Some(SnapshotRecord::decode_for(&bytes, index, snapshot)?)),
        }
    }

    /// Restore snapshot `snapshot` of `source` as `new_index`: register the new index as a holder
    /// of the snapshot and write the record that names it. Nothing is copied; the new index shares
    /// the source's objects and its key, and the source is attributed their storage until the
    /// restore is detached. Costs a snapshot read, a compare-and-swap of it, and one create.
    pub fn restore(
        &self,
        source: &Id,
        snapshot: &Id,
        new_index: &Id,
        keys: &dyn Keys,
        now_unix_ms: u64,
    ) -> Result<Restored, Error> {
        let key = self.layout.snapshot(source, snapshot);
        let origin_key = self.layout.restore_origin(new_index);
        if self.io.exists(&origin_key)? {
            return Err(Error::AlreadyExists);
        }
        let (bytes, version) = self.io.read(&key)?.ok_or(Error::NotFound)?;
        let mut record = SnapshotRecord::decode_for(&bytes, source, snapshot)?;
        // A retry by a holder is allowed on a retired snapshot: its holder mark is already there
        // and the snapshot keeps its pins for it, so finishing the restore is safe and the only
        // way to end an unfinished one (drop_restore needs the origin this writes).
        let already = record.holders.contains(new_index);
        match record.state {
            State::Active => {}
            State::Retired if already => {}
            State::Deleting | State::Retired => return Err(Error::Deleting),
        }
        for s in &record.shards {
            if let Some(r) = &s.root
                && !keys.usable(&r.crypto_envelope)
            {
                return Err(Error::KeyUnavailable);
            }
        }
        // Register as a holder first: a holder with no origin is a leak a retry mends, an origin
        // with no holder would be a reader of objects nothing keeps.
        if !already {
            if record.holders.len() >= MAX_HOLDERS {
                return Err(Error::TooManyHolders);
            }
            record.holders.push(*new_index);
            let identity = request_id(&[b"holder", key.as_bytes(), new_index])?;
            match self.io.write(
                &key,
                &record.encode(),
                Precondition::Matches(version),
                identity,
            ) {
                Ok(()) => {}
                Err(StoreError::Conflict) => return Err(Error::Contended),
                Err(e) => return Err(e.into()),
            }
        }
        let origin = OriginRecord {
            index: *new_index,
            source: *source,
            snapshot: *snapshot,
            created_unix_ms: now_unix_ms,
            shards: record.shards.clone(),
        };
        let identity = request_id(&[b"origin", origin_key.as_bytes()])?;
        match self.io.write(
            &origin_key,
            &origin.encode(),
            Precondition::Absent,
            identity,
        ) {
            Ok(()) => Ok(Restored {
                record: origin,
                requests: self.requests(),
            }),
            Err(StoreError::Conflict) => Err(Error::AlreadyExists),
            Err(e) => Err(e.into()),
        }
    }

    /// Read a restored index's origin record, or `None` when it has none.
    pub fn read_origin(&self, index: &Id) -> Result<Option<OriginRecord>, Error> {
        let key = self.layout.restore_origin(index);
        match self.io.read(&key)? {
            None => Ok(None),
            Some((bytes, _)) => Ok(Some(OriginRecord::decode_for(&bytes, index)?)),
        }
    }

    /// Release the pins of a snapshot record and remove it: the end of a delete.
    fn finish_delete(&self, key: &str, record: &SnapshotRecord) -> Result<usize, Error> {
        let pins = self.release_all(&record.index, &record.shards)?;
        if let Some((_, version)) = self.io.read(key)? {
            self.io.remove(key, &version)?;
        }
        Ok(pins)
    }

    /// Delete a snapshot. Without holders: mark it, release its pins, remove it, safe to run again
    /// after any stop. With holders it is retired instead and keeps its pins until the last
    /// restored index is dropped.
    ///
    /// What the snapshot alone held becomes collectable at the next sweep; what a live index, a
    /// restored index or another snapshot also holds does not, because each holds it with a pin
    /// of its own or through a holder.
    pub fn delete(&self, index: &Id, snapshot: &Id) -> Result<Released, Error> {
        let key = self.layout.snapshot(index, snapshot);
        let (bytes, version) = self.io.read(&key)?.ok_or(Error::NotFound)?;
        let mut record = SnapshotRecord::decode_for(&bytes, index, snapshot)?;
        if record.state == State::Retired {
            return Ok(Released {
                pins: 0,
                requests: self.requests(),
            });
        }
        if record.state == State::Active {
            record.state = if record.holders.is_empty() {
                State::Deleting
            } else {
                State::Retired
            };
            let identity = request_id(&[b"snapshot-deleting", key.as_bytes()])?;
            match self.io.write(
                &key,
                &record.encode(),
                Precondition::Matches(version),
                identity,
            ) {
                Ok(()) => {}
                Err(StoreError::Conflict) => return Err(Error::Contended),
                Err(e) => return Err(e.into()),
            }
            if record.state == State::Retired {
                return Ok(Released {
                    pins: 0,
                    requests: self.requests(),
                });
            }
        }
        let pins = self.finish_delete(&key, &record)?;
        Ok(Released {
            pins,
            requests: self.requests(),
        })
    }

    /// Drop a restored index's hold on its source and remove its origin record. When it was the
    /// last holder of a retired snapshot, the snapshot's pins are released and the objects are
    /// collectable unless something else holds them. Safe to run again after any stop.
    pub fn drop_restore(&self, index: &Id) -> Result<Released, Error> {
        let origin_key = self.layout.restore_origin(index);
        let (bytes, origin_version) = self.io.read(&origin_key)?.ok_or(Error::NotFound)?;
        let origin = OriginRecord::decode_for(&bytes, index)?;
        let key = self.layout.snapshot(&origin.source, &origin.snapshot);
        let mut pins = 0;
        if let Some((bytes, version)) = self.io.read(&key)? {
            let mut record = SnapshotRecord::decode_for(&bytes, &origin.source, &origin.snapshot)?;
            if let Some(at) = record.holders.iter().position(|h| h == index) {
                record.holders.remove(at);
                let last_of_retired = record.holders.is_empty() && record.state == State::Retired;
                if last_of_retired {
                    record.state = State::Deleting;
                }
                let identity = request_id(&[b"unhold", key.as_bytes(), index])?;
                match self.io.write(
                    &key,
                    &record.encode(),
                    Precondition::Matches(version),
                    identity,
                ) {
                    Ok(()) => {}
                    Err(StoreError::Conflict) => return Err(Error::Contended),
                    Err(e) => return Err(e.into()),
                }
                if last_of_retired {
                    pins = self.finish_delete(&key, &record)?;
                }
            }
        }
        self.io.remove(&origin_key, &origin_version)?;
        Ok(Released {
            pins,
            requests: self.requests(),
        })
    }

    /// Visit every file of one shard of a snapshot or restored index, one manifest page at a time
    /// (memory is one page), with the key of the file's object. Refused before any request when
    /// the key behind the root is revoked, which makes a snapshot and every restore of it
    /// unreadable together.
    pub fn files(
        &self,
        source: &Id,
        shard: &ShardPin,
        keys: &dyn Keys,
        mut each: impl FnMut(&FileEntry, &str) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let Some(r) = &shard.root else {
            return Ok(());
        };
        if !keys.usable(&r.crypto_envelope) {
            return Err(Error::KeyUnavailable);
        }
        let key = self
            .layout
            .manifest_root(source, &shard.shard, shard.incarnation, &r.manifest);
        let (bytes, _) = self
            .io
            .read(&key)?
            .ok_or(Error::Corrupt("a held root is missing"))?;
        let root = ManifestRoot::decode_for(&bytes, source)?;
        if root.generation != r.generation || root.crypto_envelope != r.crypto_envelope {
            return Err(Error::Corrupt("a held root is not the one recorded"));
        }
        for (i, page_ref) in root.pages.iter().enumerate() {
            let page_key =
                self.layout
                    .manifest_page(source, &shard.shard, shard.incarnation, &page_ref.page);
            let (page, _) = self
                .io
                .read(&page_key)?
                .ok_or(Error::Corrupt("a held page is missing"))?;
            for entry in &root.verify_page(i, &page)?.entries {
                each(entry, &self.layout.payload(source, &entry.object))?;
            }
        }
        Ok(())
    }
}
