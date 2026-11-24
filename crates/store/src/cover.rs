//! The lane head's cover on a lane shared by several indexes: the slot below which **no member
//! needs a record**, a lower bound the node maintains from state it already holds.
//!
//! A host is pinned from its open (or from its first record after a base) until a published base
//! covers everything it wrote. The cover is the lowest pinned slot, or the lane's tail when nothing
//! is pinned, so an idle index whose base covers all it wrote pins nothing and costs no request,
//! and no step reads the pointer of every member.
use std::collections::BTreeMap;

use hsearch_format::{Id, StructuralHead};
use hsearch_object::{MetadataStore, VersionToken};
use sha2::{Digest as _, Sha256};

use crate::publish::{Error, HeadRead, Publisher};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Pin {
    /// The lowest slot that may hold a record this index needs.
    first: u64,
    /// The highest slot an index record was written to (or `first` before any).
    last: u64,
}

/// What pins a lane's cover.
#[derive(Debug, Default)]
pub struct LaneCover {
    pins: BTreeMap<Id, Pin>,
    /// One past the highest slot any host reported: a bound on the lane's tail that never exceeds
    /// it, which is all a cover may rise to without reading the lane.
    seen: u64,
}

impl LaneCover {
    /// Nothing pinned.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin `index` from slot `from`: a host opening on a base starts at the position its recovery
    /// started from, never the lane's end. An index already pinned keeps the lower position.
    pub fn pin(&mut self, index: Id, from: u64) {
        self.seen = self.seen.max(from);
        let pin = self.pins.entry(index).or_insert(Pin {
            first: from,
            last: from,
        });
        pin.first = pin.first.min(from);
        pin.last = pin.last.max(from);
    }

    /// `index` is about to append: pin it at `seen`, which is at or below the slot the append will
    /// take (the lane's tail never falls below any slot reported). Called before the append, so a
    /// cover another host computes while this record is in flight cannot pass it.
    pub fn reserve(&mut self, index: Id) {
        let at = self.seen;
        self.pins.entry(index).or_insert(Pin {
            first: at,
            last: at,
        });
    }

    /// A record of `index` was written to `slot`: pin it there unless it is pinned lower already.
    pub fn wrote(&mut self, index: Id, slot: u64) {
        self.seen = self.seen.max(slot + 1);
        let pin = self.pins.entry(index).or_insert(Pin {
            first: slot,
            last: slot,
        });
        pin.last = pin.last.max(slot);
    }

    /// A base covering every record of `index` below slot `next` (the lane position its cut was
    /// taken at) is published and named. The index is unpinned unless it wrote at or above `next`
    /// since the cut, in which case it stays pinned from `next`, which is below every such write.
    pub fn published(&mut self, index: &Id, next: u64) {
        if let Some(pin) = self.pins.get_mut(index) {
            if pin.last < next {
                self.pins.remove(index);
            } else {
                pin.first = pin.first.max(next);
            }
        }
    }

    /// An index promoted off this lane stays pinned at its lowest uncovered slot until a base on its
    /// new lane covers its seal: `covered` is that base's covered sequence and `sealed` the last
    /// sequence the seal named. Returns whether it was released.
    pub fn release_promoted(&mut self, index: &Id, covered: u64, sealed: u64) -> bool {
        if covered > sealed {
            self.pins.remove(index).is_some()
        } else {
            false
        }
    }

    /// Stop pinning a deleted index: its seal and removed membership say no one will read its
    /// records, and a recreate has another identity.
    pub fn forget(&mut self, index: &Id) {
        self.pins.remove(index);
    }

    /// The lowest pinned slot.
    #[must_use]
    pub fn lowest(&self) -> Option<u64> {
        self.pins.values().map(|p| p.first).min()
    }

    /// The cover the pins allow on a lane whose next slot is `tail`.
    #[must_use]
    pub fn allowed(&self, tail: u64) -> u64 {
        self.lowest().map_or(tail, |low| low.min(tail))
    }

    /// Whether `index` has held its pin for `bound` slots or more by the time the lane is at `next`:
    /// it should publish a base now, so a lightly written index cannot hold the lane indefinitely.
    #[must_use]
    pub fn span_reaches(&self, index: &Id, next: u64, bound: u64) -> bool {
        self.pins
            .get(index)
            .is_some_and(|p| next.saturating_sub(p.first) >= bound)
    }

    /// The cover the pins allow below the highest slot reported so far.
    #[must_use]
    pub fn allowed_seen(&self) -> u64 {
        self.allowed(self.seen)
    }

    /// One past the highest slot reported.
    #[must_use]
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// The pinned indexes whose pin has held `bound` slots or more.
    #[must_use]
    pub fn overdue(&self, bound: u64) -> Vec<Id> {
        self.pins
            .iter()
            .filter(|(_, p)| self.seen.saturating_sub(p.first) >= bound)
            .map(|(index, _)| *index)
            .collect()
    }

    /// Whether `index` is pinned.
    #[must_use]
    pub fn is_pinned(&self, index: &Id) -> bool {
        self.pins.contains_key(index)
    }

    /// Raise the highest slot known to be written (the lane's tail as read): never lowers it.
    pub fn observe(&mut self, tail: u64) {
        self.seen = self.seen.max(tail);
    }

    /// How many indexes are pinned.
    #[must_use]
    pub fn pinned(&self) -> usize {
        self.pins.len()
    }
}

/// The checkpoint identity a shared lane's head carries: derived from the lane and the cover, with
/// no object behind it, so it rises with the cover and satisfies the head's successor rule.
#[must_use]
pub fn witness_root(lane: &hsearch_lane::LaneId, cover: u64) -> Id {
    let mut h = Sha256::new();
    h.update(b"lane-cover");
    h.update(lane.domain);
    h.update(lane.incarnation.to_le_bytes());
    h.update(cover.to_le_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(h.finalize().get(..16).unwrap_or(&[0; 16]));
    if id == [0; 16] {
        id[0] = 1;
    }
    id
}

impl<S: MetadataStore + hsearch_object::LargeObjectWriter + 'static> Publisher<S> {
    /// Raise the lane head's cover to what `pins` allow, if that is above it. Returns the new head
    /// when it moved. One head read and, when it moves, one head CAS: never a request per index.
    pub fn advance_cover(&self, pins: &SharedCover) -> Result<Option<StructuralHead>, Error> {
        let (head, version) = hsearch_lane::block_on(hsearch_lane::read_head(
            self.store(),
            self.layout(),
            pins.lane(),
        ))
        .map_err(|e| Error::Head(format!("{e:?}")))?;
        let target = pins.allowed();
        if target <= head.cover {
            return Ok(None);
        }
        let at = HeadRead {
            lane: pins.lane(),
            head: &head,
            version: &version,
        };
        self.raise_cover(&at, target, pins.writer_epoch())
            .map(|(next, _)| Some(next))
    }

    /// Raise the head's cover to `cover` (the lane's [`LaneCover::allowed`]), naming the witness
    /// root. `writer_epoch` is the epoch the cover was computed under: a head whose epoch has moved
    /// on belongs to a newer writer, which may have pinned records this one has not seen, so the
    /// raise is refused ([`Error::Lost`]). A compare-and-swap on the version read, like any head
    /// transition. A cover not above the head's is [`Error::Mismatch`].
    pub fn raise_cover(
        &self,
        at: &HeadRead<'_>,
        cover: u64,
        writer_epoch: u64,
    ) -> Result<(StructuralHead, VersionToken), Error> {
        if at.head.writer_epoch != writer_epoch {
            return Err(Error::Lost);
        }
        if cover <= at.head.cover {
            return Err(Error::Mismatch);
        }
        self.advance(
            at,
            hsearch_lane::Change::Checkpoint {
                cover,
                root: witness_root(at.lane, cover),
            },
        )
    }
}

/// The pinned set of one lane, shared by the hosts that write it.
#[derive(Debug)]
pub struct SharedCover {
    lane: hsearch_lane::LaneId,
    writer_epoch: u64,
    span_bound: u64,
    pins: std::sync::Mutex<LaneCover>,
}

impl SharedCover {
    /// The pins of a lane known to hold nothing no base covers (a new lane); a lane that has been
    /// written is opened with [`SharedCover::open`], which scans its tail first. The writer of
    /// `writer_epoch` writes it. A host publishes a base once
    /// its pin has held `span_bound` slots, so a lightly written index cannot hold the lane.
    #[must_use]
    pub fn new(lane: hsearch_lane::LaneId, writer_epoch: u64, span_bound: u64) -> Self {
        Self {
            lane,
            writer_epoch,
            span_bound,
            pins: std::sync::Mutex::new(LaneCover::new()),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut LaneCover) -> R) -> R {
        // A panic while the pins were held leaves them as they were; recovering the lock keeps the
        // set (dropping it would unpin everything and let the cover pass a record).
        f(&mut self
            .pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// The pinned set of a lane a node is opening: the tail above the head's cover is scanned
    /// first, so every index with a record its base does not cover pins the lane from its first
    /// such slot before any host opens, and the cover cannot pass what an index not yet opened
    /// still needs. `shard_of` maps an index to the shard and incarnation its pointer is under.
    ///
    /// # Errors
    /// [`crate::tail::Error`] if the tail cannot be read or is over `max_records`.
    pub fn open<S, F>(
        publisher: &Publisher<S>,
        lane: hsearch_lane::LaneId,
        writer_epoch: u64,
        span_bound: u64,
        max_records: usize,
        shard_of: F,
    ) -> Result<Self, crate::tail::Error>
    where
        S: MetadataStore + hsearch_object::LargeObjectWriter + 'static,
        F: Fn(&Id) -> (Id, u64),
    {
        let tail = crate::tail::scan(publisher, &lane, max_records, shard_of)?;
        let this = Self::new(lane, writer_epoch, span_bound);
        this.with(|c| {
            c.observe(tail.end);
            for t in &tail.indexes {
                c.pin(t.index, t.first);
            }
        });
        Ok(this)
    }

    /// See [`LaneCover::reserve`].
    pub fn reserve(&self, index: Id) {
        self.with(|c| c.reserve(index));
    }

    /// See [`LaneCover::is_pinned`].
    #[must_use]
    pub fn is_pinned(&self, index: &Id) -> bool {
        self.with(|c| c.is_pinned(index))
    }

    /// See [`LaneCover::forget`].
    pub fn forget(&self, index: &Id) {
        self.with(|c| c.forget(index));
    }

    /// See [`LaneCover::release_promoted`].
    pub fn release_promoted(&self, index: &Id, covered: u64, sealed: u64) -> bool {
        self.with(|c| c.release_promoted(index, covered, sealed))
    }

    /// See [`LaneCover::pin`].
    pub fn pin(&self, index: Id, from: u64) {
        self.with(|c| c.pin(index, from));
    }

    /// See [`LaneCover::wrote`].
    pub fn wrote(&self, index: Id, slot: u64) {
        self.with(|c| c.wrote(index, slot));
    }

    /// See [`LaneCover::published`].
    pub fn published(&self, index: &Id, next: u64) {
        self.with(|c| c.published(index, next));
    }

    /// Whether `index` should publish a base now.
    #[must_use]
    pub fn due(&self, index: &Id) -> bool {
        self.with(|c| c.span_reaches(index, c.seen(), self.span_bound))
    }

    /// The cover the pins allow now.
    #[must_use]
    pub fn allowed(&self) -> u64 {
        self.with(|c| c.allowed_seen())
    }

    /// The pinned indexes whose pin has held the span bound: they should publish a base now. A node
    /// asks after a commit on the lane, so an index that stopped writing is still reached.
    #[must_use]
    pub fn overdue(&self) -> Vec<Id> {
        self.with(|c| c.overdue(self.span_bound))
    }

    /// The lane these pins belong to.
    #[must_use]
    pub fn lane(&self) -> &hsearch_lane::LaneId {
        &self.lane
    }

    /// The writer epoch the pins were computed under.
    #[must_use]
    pub fn writer_epoch(&self) -> u64 {
        self.writer_epoch
    }
}
