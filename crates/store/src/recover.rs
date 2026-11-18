//! Recovering one index from the object store alone: nothing local is consulted.
//!
//! Recovery reads the lane's structural head, verifies the base manifest it names (if any), then
//! streams the committed log from the head's cover to the first absent slot, handing this index's
//! records to the caller in sequence order. It refuses to call the index ready unless the history
//! is whole: a sequence gap, a different body under a repeated identity, a corrupt slot, a log that
//! ends below a position the caller knows was acknowledged, or a replay larger than its bounds each
//! block readiness with a named reason. A record below the base is accepted without comparison
//! (the base holds it).
//!
//! Memory is one slot, one external document at a time, and a dense array of body digests (32 bytes
//! a record) that stays in memory up to [`ReplayLimits::records`]. Past it a [`Spill::Scratch`]
//! setting moves the rest of the array to a scratch file, so a long tail replays in constant
//! memory instead of being refused; [`recover`] keeps the refusal, [`recover_with`] opts in.
//!
//! A put whose document is too large for its slot names an object: it is fetched (one HEAD, then
//! ranged GETs of at most [`PAYLOAD_CHUNK`] bytes) and its length and SHA-256 are verified before
//! the record is applied. A sealed index is followed to the lane it was promoted to, so the applied
//! prefix reaches the true end of its history without the caller knowing the sequence.
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hsearch_format::{Digest, Id, PayloadRef, RecordOp, RecordRef, SlotKind, StructuralHead};
use hsearch_index::barrier::{Replay, Step};
use hsearch_lane::{Error as LaneError, LaneId, Route, block_on, read_head, read_log, route};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error as ObjectError, LargeObjectWriter, MetadataStore,
    ObjectKey, RequestContext,
};
use sha2::{Digest as _, Sha256};

use crate::publish::{Dependencies, Error as PublishError, Publisher};

/// Bounds on one recovery's replay: a larger tail must be checkpointed instead of replayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplayLimits {
    /// Most slots read.
    pub slots: u64,
    /// Most of this index's records applied.
    pub records: u64,
    /// Most body bytes of this index's records applied.
    pub bytes: u64,
}

/// What recovery must reach before it may call the index ready.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Expect {
    /// A lane position the log must reach: the first absent slot must be at or above it. Taken
    /// from the highest acknowledged receipt the caller knows (or an authority barrier).
    pub ordinal: u64,
    /// The index sequence the replay must reach: the applied prefix must be at least this.
    pub seq: u64,
}

/// What to recover.
pub struct Target<'a> {
    /// The lane the index lives in.
    pub lane: &'a LaneId,
    /// The index.
    pub index: Id,
    /// The shard of the index's manifests.
    pub shard: Id,
    /// The shard incarnation.
    pub incarnation: u64,
    /// Replay bounds.
    pub limits: ReplayLimits,
    /// What must be reached.
    pub expect: Expect,
}

/// One committed record handed to the caller, in sequence order.
pub struct Record<'a> {
    /// The index sequence.
    pub seq: u64,
    /// Whether this deletes the document.
    pub delete: bool,
    /// The body (empty for a delete).
    pub body: &'a [u8],
    /// The slot it was found in, in the lane it was read from (a successor lane's ordinals restart).
    pub ordinal: u64,
}

/// Why the index is not ready. Every variant blocks readiness; none is guessed around.
#[derive(Debug, Eq, PartialEq)]
pub enum NotReady {
    /// The lane has no structural head.
    NoHead,
    /// The head authorises a different activation than the one asked for.
    Activation,
    /// The base manifest the head names cannot be verified.
    Base(String),
    /// A sequence of this index is missing: `expected` was next, `found` came.
    Gap {
        /// The sequence that was due.
        expected: u64,
        /// The sequence that arrived.
        found: u64,
    },
    /// A sequence appears twice with different contents.
    Conflict(u64),
    /// A slot is not a valid lane object.
    Corrupt,
    /// The log ends below a position known to be committed, or the applied prefix is below the
    /// expected sequence.
    Missing,
    /// The replay is over a bound: take a checkpoint.
    Limit,
    /// The caller's apply failed.
    Apply,
    /// The external payload of this sequence is absent, the wrong length or fails its digest:
    /// committed history is lost, and skipping the record would lose a document.
    Payload(u64),
    /// The scratch file a spill needs could not be used.
    Scratch(String),
    /// The store failed; retry.
    Store(String),
}

/// A recovered index.
#[derive(Debug, Eq, PartialEq)]
pub struct Recovered {
    /// The base manifest replayed on top of, if the head names one.
    pub base: Option<hsearch_format::ManifestRoot>,
    /// The first sequence of this index not in the base.
    pub from_seq: u64,
    /// The applied prefix: sequences below this are in the base or were applied.
    pub applied: u64,
    /// Records of this index applied from the log.
    pub replayed: u64,
    /// Slots read.
    pub slots: u64,
    /// The first absent position of the lane recovery started in (not of a successor lane it
    /// followed: `followed > 0` means the history ends elsewhere).
    pub tail: u64,
    /// The head generation recovery started from.
    pub head_generation: u64,
    /// Successor lanes followed after a seal (0 when the index never moved).
    pub followed: u32,
    /// Digests that went to the scratch file (0 unless a spill was needed).
    pub spilled: u64,
}

/// What recovery does with a replay larger than [`ReplayLimits`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Spill {
    /// Refuse it ([`NotReady::Limit`]): take a checkpoint instead.
    Refuse,
    /// Keep going, holding the digests past `limits.records` in a scratch file in `dir`. The
    /// limits become the in-memory bounds; `ceiling` is what may still be replayed (each field is
    /// raised to at least the limit's). The file is created on first use and removed on return.
    Scratch {
        /// A local directory with room for 32 bytes a record.
        dir: PathBuf,
        /// The most that may be replayed with a spill.
        ceiling: ReplayLimits,
    },
}

/// Most bytes of one external payload read per request: requests scale with bytes, memory holds
/// one chunk beside the document being assembled.
pub const PAYLOAD_CHUNK: u64 = 8 << 20;
/// Most lanes one recovery follows (a sealed index's chain of promotions).
const MAX_HOPS: u32 = 16;
/// Digests buffered before a scratch write.
const FLUSH: usize = 2048;
const DEADLINE: Duration = Duration::from_secs(60);

fn body_digest(delete: bool, body: &[u8]) -> Digest {
    let mut h = Sha256::new();
    h.update([u8::from(delete)]);
    h.update(body);
    h.finalize().into()
}

fn scratch(e: &std::io::Error) -> NotReady {
    NotReady::Scratch(e.to_string())
}

/// The scratch file; removed when recovery returns.
struct SpillFile {
    file: File,
    path: PathBuf,
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The digest of every applied sequence, indexed by `seq - from`. Applied sequences are
/// consecutive, so the array is dense: the first `cap` entries live in memory and the rest in the
/// scratch file at `32 * (index - cap)`, read back only when a sequence repeats.
struct Digests {
    from: u64,
    cap: usize,
    dir: Option<PathBuf>,
    mem: Vec<Digest>,
    buf: Vec<Digest>,
    flushed: u64,
    file: Option<SpillFile>,
}

impl Digests {
    fn new(from: u64, cap: u64, dir: Option<PathBuf>) -> Self {
        Self {
            from,
            cap: usize::try_from(cap).unwrap_or(usize::MAX),
            dir,
            mem: Vec::new(),
            buf: Vec::new(),
            flushed: 0,
            file: None,
        }
    }

    fn spilled(&self) -> u64 {
        self.flushed + self.buf.len() as u64
    }

    fn insert(&mut self, seq: u64, digest: Digest) -> Result<(), NotReady> {
        debug_assert_eq!(seq - self.from, (self.mem.len() as u64) + self.spilled());
        if self.mem.len() < self.cap {
            self.mem.push(digest);
            return Ok(());
        }
        if self.dir.is_none() {
            return Err(NotReady::Limit);
        }
        self.buf.push(digest);
        if self.buf.len() >= FLUSH {
            self.flush()?;
        }
        Ok(())
    }

    fn open(&mut self) -> Result<&mut SpillFile, NotReady> {
        if self.file.is_none() {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = self.dir.as_ref().ok_or(NotReady::Limit)?;
            let mut last = None;
            for _ in 0..8 {
                let name = format!(
                    "hsearch-recover-{}-{}.digests",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                );
                let path = dir.join(name);
                match File::options()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    Ok(file) => {
                        self.file = Some(SpillFile { file, path });
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = Some(e),
                    Err(e) => return Err(scratch(&e)),
                }
            }
            if self.file.is_none() {
                return Err(last.map_or(NotReady::Limit, |e| scratch(&e)));
            }
        }
        self.file.as_mut().ok_or(NotReady::Limit)
    }

    fn flush(&mut self) -> Result<(), NotReady> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let at = self.flushed * 32;
        let n = self.buf.len() as u64;
        let bytes: Vec<u8> = self.buf.iter().flatten().copied().collect();
        let file = &mut self.open()?.file;
        file.seek(SeekFrom::Start(at)).map_err(|e| scratch(&e))?;
        file.write_all(&bytes).map_err(|e| scratch(&e))?;
        self.flushed += n;
        self.buf.clear();
        Ok(())
    }

    /// The digest recorded for `seq`, if this replay applied it.
    fn get(&mut self, seq: u64) -> Result<Option<Digest>, NotReady> {
        let Some(index) = seq.checked_sub(self.from) else {
            return Ok(None);
        };
        if index < self.mem.len() as u64 {
            return Ok(Some(self.mem[index as usize]));
        }
        let at = index - self.mem.len() as u64;
        if at >= self.spilled() {
            return Ok(None);
        }
        if at >= self.flushed {
            return Ok(Some(self.buf[(at - self.flushed) as usize]));
        }
        let file = &mut self.open()?.file;
        file.seek(SeekFrom::Start(at * 32))
            .map_err(|e| scratch(&e))?;
        let mut digest = [0u8; 32];
        file.read_exact(&mut digest).map_err(|e| scratch(&e))?;
        Ok(Some(digest))
    }
}

/// How a record's document is held.
enum Body<'a> {
    Inline(&'a [u8]),
    Delete,
    External {
        object: Id,
        length: u64,
        digest: Digest,
    },
}

/// One recovery's running state across the lanes it reads.
struct Run<'a, S, F> {
    publisher: &'a Publisher<S>,
    index: Id,
    hard: ReplayLimits,
    replay: Replay,
    seen: Digests,
    apply: F,
    slots: u64,
    replayed: u64,
    bytes: u64,
    /// Bytes fetched only to compare a repeated external record; bounded like `bytes`.
    repeated: u64,
    /// The buffer an external document is assembled in, kept between records.
    payload: Vec<u8>,
    /// The sequence the index was sealed at in the lane being read.
    sealed: Option<u64>,
    failure: Option<NotReady>,
}

impl<S, F> Run<'_, S, F>
where
    S: MetadataStore + LargeObjectWriter + 'static,
    F: FnMut(&Record<'_>) -> Result<(), ()>,
{
    fn slot<'v>(
        &mut self,
        kind: SlotKind,
        ordinal: u64,
        records: impl Iterator<Item = RecordRef<'v>>,
    ) -> Result<(), NotReady> {
        self.slots += 1;
        if self.slots > self.hard.slots {
            return Err(NotReady::Limit);
        }
        if kind != SlotKind::Data {
            return Ok(());
        }
        for record in records {
            if record.index == self.index {
                self.record(ordinal, &record)?;
            }
        }
        Ok(())
    }

    fn record(&mut self, ordinal: u64, record: &RecordRef<'_>) -> Result<(), NotReady> {
        let body = match (record.op, record.payload) {
            // A seal repeats the last sequence and carries no document; nothing may follow it.
            (RecordOp::Seal, _) => {
                return match self.sealed {
                    Some(at) if at != record.seq => Err(NotReady::Corrupt),
                    _ => {
                        self.sealed = Some(record.seq);
                        Ok(())
                    }
                };
            }
            _ if self.sealed.is_some() => return Err(NotReady::Corrupt),
            (RecordOp::Put, PayloadRef::Inline(b)) => Body::Inline(b),
            (RecordOp::Delete, PayloadRef::None) => Body::Delete,
            (
                RecordOp::Put,
                PayloadRef::External {
                    object,
                    length,
                    digest,
                },
            ) => Body::External {
                object,
                length,
                digest,
            },
            _ => return Err(NotReady::Corrupt),
        };
        let seq = record.seq;
        match self.replay.begin(seq) {
            Ok(Step::Apply) => {
                self.replayed += 1;
                self.bytes += match body {
                    Body::Inline(b) => b.len() as u64,
                    Body::Delete => 0,
                    // Counted before the fetch, so a refused document costs no request.
                    Body::External { length, .. } => length,
                };
                if self.replayed > self.hard.records || self.bytes > self.hard.bytes {
                    return Err(NotReady::Limit);
                }
                let mut held = Vec::new();
                let (delete, body): (bool, &[u8]) = match body {
                    Body::Inline(b) => (false, b),
                    Body::Delete => (true, &[]),
                    Body::External {
                        object,
                        length,
                        digest,
                    } => {
                        held = std::mem::take(&mut self.payload);
                        self.fetch(&mut held, seq, &object, length, &digest)?;
                        (false, &held)
                    }
                };
                let digest = body_digest(delete, body);
                let applied = Record {
                    seq,
                    delete,
                    body,
                    ordinal,
                };
                if (self.apply)(&applied).is_err() || self.replay.done(seq).is_err() {
                    self.replay.fail();
                    return Err(NotReady::Apply);
                }
                self.seen.insert(seq, digest)?;
                // A large document's buffer is not kept for the next record.
                held.clear();
                if held.capacity() <= (PAYLOAD_CHUNK as usize) {
                    self.payload = held;
                }
                Ok(())
            }
            Ok(Step::Duplicate) => {
                // Seen in this replay with other contents is corruption; below the base there is
                // nothing to compare against and the base already holds it, so nothing is fetched.
                let Some(previous) = self.seen.get(seq)? else {
                    return Ok(());
                };
                let digest = match body {
                    Body::Inline(b) => body_digest(false, b),
                    Body::Delete => body_digest(true, &[]),
                    Body::External {
                        object,
                        length,
                        digest,
                    } => {
                        self.repeated += length;
                        if self.repeated > self.hard.bytes {
                            return Err(NotReady::Limit);
                        }
                        let mut held = std::mem::take(&mut self.payload);
                        self.fetch(&mut held, seq, &object, length, &digest)?;
                        let d = body_digest(false, &held);
                        held.clear();
                        if held.capacity() <= (PAYLOAD_CHUNK as usize) {
                            self.payload = held;
                        }
                        d
                    }
                };
                if previous == digest {
                    Ok(())
                } else {
                    Err(NotReady::Conflict(seq))
                }
            }
            Ok(Step::Gap { expected }) => Err(NotReady::Gap {
                expected,
                found: seq,
            }),
            Err(_) => Err(NotReady::Apply),
        }
    }

    /// Read the payload object into `buf`: one HEAD for its version and length, then ranged GETs
    /// of at most [`PAYLOAD_CHUNK`], hashing as they arrive. A transient store failure is
    /// [`NotReady::Store`]; an absent, short, long or altered object is [`NotReady::Payload`].
    fn fetch(
        &self,
        buf: &mut Vec<u8>,
        seq: u64,
        object: &Id,
        length: u64,
        digest: &Digest,
    ) -> Result<(), NotReady> {
        let store = self.publisher.store();
        let key = self.publisher.layout().payload(&self.index, object);
        let key = ObjectKey::new(key.as_bytes()).map_err(|_| NotReady::Corrupt)?;
        let ctx = |response: u64| {
            RequestContext::new(
                DEADLINE,
                Cancellation::default(),
                Budget {
                    request_bytes: 4096,
                    response_bytes: 4096 + response,
                    attempts: 8,
                },
            )
            .map_err(|e| NotReady::Store(format!("{e:?}")))
        };
        let read = |e: ObjectError| match e {
            ObjectError::NotFound => NotReady::Payload(seq),
            other => NotReady::Store(format!("{other:?}")),
        };
        let info = block_on(store.head(&ctx(0)?, &key)).map_err(read)?;
        let total = usize::try_from(length).map_err(|_| NotReady::Limit)?;
        if info.length != length {
            return Err(NotReady::Payload(seq));
        }
        buf.clear();
        buf.reserve_exact(total);
        let mut hash = Sha256::new();
        let mut at = 0u64;
        while at < length {
            let take = PAYLOAD_CHUNK.min(length - at);
            let range = ByteRange::new(at, take).map_err(|_| NotReady::Corrupt)?;
            let part = block_on(store.read_range(&ctx(take)?, &key, &info.version, range))
                .map_err(read)?;
            if part.as_bytes().len() as u64 != take {
                return Err(NotReady::Payload(seq));
            }
            hash.update(part.as_bytes());
            buf.extend_from_slice(part.as_bytes());
            at += take;
        }
        if Digest::from(hash.finalize()) != *digest {
            return Err(NotReady::Payload(seq));
        }
        Ok(())
    }
}

fn head_error(e: LaneError) -> NotReady {
    match e {
        LaneError::NoHead => NotReady::NoHead,
        LaneError::Format(_) => NotReady::Corrupt,
        other => NotReady::Store(format!("{other:?}")),
    }
}

/// Recover `target`, calling `apply` on each of its records after the base, in sequence order.
/// A replay over `target.limits` is refused ([`Spill::Refuse`]); see [`recover_with`].
pub fn recover<S, D, F>(
    publisher: &Publisher<S>,
    target: &Target<'_>,
    dependencies: &D,
    apply: F,
) -> Result<Recovered, NotReady>
where
    S: MetadataStore + LargeObjectWriter + 'static,
    D: Dependencies,
    F: FnMut(&Record<'_>) -> Result<(), ()>,
{
    recover_with(publisher, target, dependencies, &Spill::Refuse, apply)
}

/// [`recover`] with a choice of what to do with a replay over the limits.
pub fn recover_with<S, D, F>(
    publisher: &Publisher<S>,
    target: &Target<'_>,
    dependencies: &D,
    spill: &Spill,
    apply: F,
) -> Result<Recovered, NotReady>
where
    S: MetadataStore + LargeObjectWriter + 'static,
    D: Dependencies,
    F: FnMut(&Record<'_>) -> Result<(), ()>,
{
    let store = publisher.store();
    let layout = publisher.layout();
    let (head, _): (StructuralHead, _) =
        block_on(read_head(store, layout, target.lane)).map_err(head_error)?;
    if head.activation != target.lane.activation {
        return Err(NotReady::Activation);
    }
    let base = match head.checkpoint {
        None => None,
        Some(manifest) => Some(
            publisher
                .verify_root(
                    &target.index,
                    &target.shard,
                    target.incarnation,
                    &manifest,
                    dependencies,
                )
                .map_err(|e| match e {
                    // An object the base names is absent: committed history is missing, which is
                    // not a transient failure to retry.
                    PublishError::Store(hsearch_object::Error::NotFound) => {
                        NotReady::Base(format!("{e:?}"))
                    }
                    PublishError::Store(_) => NotReady::Store(format!("{e:?}")),
                    other => NotReady::Base(format!("{other:?}")),
                })?,
        ),
    };
    let from_seq = base.as_ref().map_or(0, |b| b.covered);
    let (hard, dir) = match spill {
        Spill::Refuse => (target.limits, None),
        Spill::Scratch { dir, ceiling } => (
            ReplayLimits {
                slots: ceiling.slots.max(target.limits.slots),
                records: ceiling.records.max(target.limits.records),
                bytes: ceiling.bytes.max(target.limits.bytes),
            },
            Some(dir.clone()),
        ),
    };
    let mut run = Run {
        publisher,
        index: target.index,
        hard,
        replay: Replay::new(from_seq),
        seen: Digests::new(from_seq, target.limits.records, dir),
        apply,
        slots: 0,
        replayed: 0,
        bytes: 0,
        repeated: 0,
        payload: Vec::new(),
        sealed: None,
        failure: None,
    };
    let (head_generation, source_tail) = (head.generation, head.tail.ordinal);
    let mut lane = *target.lane;
    let mut head = head;
    let mut first_tail = 0;
    let mut followed = 0u32;
    loop {
        run.sealed = None;
        let end = block_on(read_log(
            store,
            layout,
            &lane,
            head.cover,
            None,
            |view| match run.slot(view.kind, view.position.ordinal, view.records()) {
                Ok(()) => Ok(()),
                Err(reason) => {
                    run.failure = Some(reason);
                    Err(LaneError::Invalid("recovery stopped"))
                }
            },
        ));
        let tail = match end {
            Ok(end) => end,
            Err(e) => {
                return Err(run.failure.take().unwrap_or(match e {
                    LaneError::Format(_) => NotReady::Corrupt,
                    other => NotReady::Store(format!("{other:?}")),
                }));
            }
        };
        // A hole is the end of the log only if no checkpoint has since reclaimed it: re-read the
        // head and refuse if it moved past where the scan stopped.
        let (after, _): (StructuralHead, _) =
            block_on(read_head(store, layout, &lane)).map_err(|e| match e {
                LaneError::NoHead => NotReady::Missing,
                other => head_error(other),
            })?;
        if after.cover > tail || after.activation != head.activation {
            return Err(NotReady::Missing);
        }
        if followed == 0 {
            first_tail = tail;
        }
        let known = if followed == 0 {
            source_tail
        } else {
            head.tail.ordinal
        };
        if tail < known {
            return Err(NotReady::Missing);
        }
        let Some(sealed) = run.sealed else { break };
        // The seal names the index's last sequence in this lane (callers have given the last one
        // held and the next one due, and an index sealed before any write names 0): the history
        // must come within one of it, or a slot is missing. A successor that starts one past a
        // missing last sequence is caught by the replay as a gap.
        let applied = run.replay.applied();
        if applied < sealed {
            return Err(NotReady::Gap {
                expected: applied,
                found: sealed,
            });
        }
        // Between the successor's establish and the index leaving the source, and for a seal whose
        // promotion never reached its head, nothing can follow the seal: the history ends here.
        let next = match block_on(route(store, layout, &lane, &target.index)) {
            Ok(Route::Promoted { lane: next, .. }) => next,
            Ok(Route::Shared(_) | Route::Unrouted) => break,
            Err(e) => return Err(head_error(e)),
        };
        followed += 1;
        if followed > MAX_HOPS {
            return Err(NotReady::Corrupt);
        }
        let (next_head, _): (StructuralHead, _) =
            block_on(read_head(store, layout, &next)).map_err(head_error)?;
        if next_head.activation != next.activation {
            return Err(NotReady::Activation);
        }
        lane = next;
        head = next_head;
    }
    if first_tail < target.expect.ordinal || run.replay.applied() < target.expect.seq {
        return Err(NotReady::Missing);
    }
    let spilled = run.seen.spilled();
    Ok(Recovered {
        base,
        from_seq,
        applied: run.replay.applied(),
        replayed: run.replayed,
        slots: run.slots,
        tail: first_tail,
        head_generation,
        followed,
        spilled,
    })
}
