//! The fenced, ordered slot log: one writer, one append in flight, one conditional create per
//! batch, and a retained fence that seals a predecessor out.
//!
//! **Commit rule.** A batch is acknowledged only after the conditional create of its slot at the
//! next position succeeded (or was resolved to ours by its identity). Positions are consecutive, so
//! the first absent position is the only place any writer can still append. **Seal rule.** A writer
//! that opens replays from its caller's cover to the first absent position and creates a *fence*
//! there; the predecessor's late create at that position is refused, and it never acknowledges
//! behind the successor's view. Fences are never deleted, because a late create into a deleted
//! fence would succeed. **Poison rule.** A refused position or an unresolved reply stops the writer
//! for good; recovery is a new `open`, which replays whatever did land.
//!
//! No LIST anywhere: every key is derived from the lane id and an ordinal.
use std::collections::BTreeMap;
use std::sync::Arc;

use hsearch_format::{
    CommitToken, Id, LaneDescriptor, Layout, MAX_SLOT_BODY_BYTES, MAX_SLOT_RECORDS, RecordOp,
    RecordSpec, SlotBody, SlotKind, SlotPosition, SlotView, encode_slot,
};
use hsearch_object::{MetadataStore, Precondition};

use crate::cover::{raise_cover, read_cover};
use crate::head::{Advance, Change, advance_head, read_head};
use crate::io::{Written, fresh_token, key, mutation, read_object, request_id, write};
use crate::members::{LaneId, read_descriptor, swap_descriptor, without_member};
use crate::{Error, Result};

/// A seal that keeps losing the race to a live writer gives up after this many tries.
const MAX_SEAL_RACES: u32 = 8;
/// A slot object is read whole; this bounds it before the read (body bound plus envelope).
/// Seals one writer remembers; each promotion of a lane adds one, and a reopen rebuilds from the
/// cover, so this bounds a single long-lived writer, not the lane.
const MAX_SEALED: usize = 1 << 16;
/// Offset of the slot-kind byte: envelope header 40, then domain 16, activation 8, lane 4, ordinal 8
/// and token 16. Kind 2 is a fence.
const FENCE_KIND_AT: u64 = 40 + 16 + 8 + 4 + 8 + 16;
/// Scratch capacity a writer keeps between appends.
const SCRATCH_KEEP: usize = 1 << 20;
const SLOT_READ_BOUND: u64 = MAX_SLOT_BODY_BYTES as u64 + 4096;

/// What opening replayed and where the writer now stands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenReport {
    /// The first position replayed (the caller's cover).
    pub from: u64,
    /// The position of the fence this open created: the log's seal.
    pub fence_ordinal: u64,
    /// Slots read (data and fences), each one HEAD and one ranged GET.
    pub slots_replayed: u64,
    /// Of those, fences left by earlier openers.
    pub fences_replayed: u64,
    /// Records in the replayed data slots.
    pub records_replayed: u64,
    /// Fence creates that lost to a live writer before one won.
    pub seal_races: u32,
    /// For [`LaneWriter::open_with_head`]: the writer epoch the open established in the head, and
    /// the head generation that recorded it. Both zero for a marker-driven open.
    pub head_epoch: u64,
    /// See [`OpenReport::head_epoch`].
    pub head_generation: u64,
}

/// An acknowledged append.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ack {
    /// The position the batch committed at.
    pub ordinal: u64,
    /// Records in the slot.
    pub records: usize,
    /// Encoded slot size in bytes.
    pub bytes: usize,
}

/// Counters a writer keeps for itself; bounded, no per-index state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WriterStats {
    /// Slots acknowledged.
    pub slots: u64,
    /// Records acknowledged.
    pub records: u64,
    /// Encoded bytes acknowledged.
    pub bytes: u64,
}

fn position(id: &LaneId, ordinal: u64) -> SlotPosition {
    SlotPosition {
        domain: id.domain,
        activation: id.activation,
        lane: id.lane,
        ordinal,
    }
}

fn slot_key(layout: &Layout, id: &LaneId, ordinal: u64) -> String {
    layout.lane_slot(&id.domain, id.incarnation, id.activation, id.lane, ordinal)
}

/// Read slot `ordinal` whole (HEAD plus ranged GET), or `None` if it does not exist.
async fn read_slot<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
    ordinal: u64,
) -> Result<Option<hsearch_object::Payload>> {
    let k = key(&slot_key(layout, id, ordinal))?;
    Ok(read_object(store, &k, SLOT_READ_BOUND)
        .await?
        .map(|(bytes, _)| bytes))
}

/// What a reclaim removed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reclaimed {
    /// Data slots removed.
    pub removed: u64,
    /// Fences found and kept.
    pub fences_kept: u64,
    /// Positions already absent (removed by an earlier run).
    pub absent: u64,
}

/// Replay slots `from..` in order, calling `visit` on each validated slot, until the first absent
/// position or `to` (exclusive). Returns that position. Memory is one slot at a time. `from` must be
/// the durable cover or above: slots below it may have been reclaimed, and a hole there reads as
/// the end of the log.
pub async fn read_log<S, F>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
    from: u64,
    to: Option<u64>,
    mut visit: F,
) -> Result<u64>
where
    S: MetadataStore,
    F: FnMut(&SlotView<'_>) -> Result<()>,
{
    let mut at = from;
    while to.is_none_or(|end| at < end) {
        let Some(bytes) = read_slot(store, layout, id, at).await? else {
            break;
        };
        let view = SlotView::parse_at(bytes.as_bytes(), &position(id, at))?;
        visit(&view)?;
        at = at.checked_add(1).ok_or(Error::Invalid("ordinal"))?;
    }
    Ok(at)
}

/// Where an open learns the cover it must not replay below.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoverSource {
    /// The lane's reclaim marker (`cover.rs`).
    Marker,
    /// The lane's structural head.
    Head,
}

/// Most head transitions one establish tries before giving up.
const MAX_ESTABLISH_ATTEMPTS: u32 = 8;

/// Record the fence this open created as a new writer epoch in the head. A head that already
/// holds a fence at or above ours means a later opener won: this open has no authority.
async fn establish<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
    fence: u64,
) -> Result<(u64, u64)> {
    for _ in 0..MAX_ESTABLISH_ATTEMPTS {
        let (head, version) = read_head(store, layout, id).await?;
        // The head has moved to a later activation: this lane id no longer holds authority.
        if id.activation < head.activation {
            return Err(Error::Fenced);
        }
        // A lost reply that did land: the head already holds exactly our epoch.
        if let Some(entry) = head.inventory.last()
            && entry.activation == id.activation
            && entry.lane == id.lane
        {
            if entry.fence == fence {
                return Ok((entry.epoch, head.generation));
            }
            if entry.fence > fence {
                return Err(Error::Fenced);
            }
        }
        if fence < head.cover {
            return Err(Error::BadCover);
        }
        let change = Change::Establish {
            activation: id.activation,
            lane: id.lane,
            fence,
        };
        match advance_head(store, layout, id, &head, &version, change).await {
            Ok(Advance::Done(next, _)) => return Ok((next.writer_epoch, next.generation)),
            Ok(Advance::Lost) | Err(Error::Contended) => {}
            Err(Error::Format(_))
                if head.inventory.len() >= hsearch_format::MAX_EPOCH_INVENTORY =>
            {
                return Err(Error::Capacity);
            }
            Err(error) => return Err(error),
        }
    }
    Err(Error::Contended)
}

/// The single writer of one lane.
pub struct LaneWriter<S> {
    store: Arc<S>,
    layout: Layout,
    id: LaneId,
    next: u64,
    /// Which record this writer's cover lives in; the other reclaim entry point is refused.
    source: CoverSource,
    descriptor: LaneDescriptor,
    version: hsearch_object::VersionToken,
    sealed: BTreeMap<Id, u64>,
    poisoned: bool,
    stats: WriterStats,
    scratch: Vec<u8>,
    /// Everything below this has been reclaimed or was below the open's cover.
    reclaimed_to: u64,
}

impl<S: MetadataStore> LaneWriter<S> {
    /// Become the lane's writer: read the descriptor, replay from `from` (the durable cover) to the
    /// first absent position, and seal it with a fence. `visit` sees every replayed slot, fences
    /// included, in order; its error aborts the open.
    ///
    /// Cost: 2 requests for the descriptor, 2 per replayed slot, 1 HEAD for the end, 1 PUT for the
    /// fence. The caller owns `from`: it must be at or above the lane's reclaim cover.
    pub async fn open<F>(
        store: Arc<S>,
        layout: Layout,
        id: LaneId,
        from: u64,
        visit: F,
    ) -> Result<(Self, OpenReport)>
    where
        F: FnMut(&SlotView<'_>) -> Result<()>,
    {
        Self::open_at(store, layout, id, from, CoverSource::Marker, visit).await
    }

    /// Become the lane's writer with the cover and the writer epoch held by the lane's structural
    /// head ([`crate::head`]) instead of the reclaim marker: replay starts at the head's cover, the
    /// fence is created as in [`LaneWriter::open`], and one head transition then records the new
    /// writer epoch, its fence and the tail above it. The reclaim marker is neither read nor
    /// written, so a lane is driven by the head **or** by the marker, never both.
    ///
    /// Cost over `open` with no marker: 2 requests to read the head, 2 to re-read it after the
    /// replay (the cover must not have moved above the end that was found), 2 more to read it
    /// inside the establish, 1 PUT for the transition, and no marker read. A seal race adds the
    /// replayed slot's 2 requests and another head re-read. The mode is kept on the writer:
    /// the reclaim entry point of the other mode is refused with [`Error::Invalid`].
    ///
    /// Errors: [`Error::NoHead`] with no head; [`Error::BadCover`] when a checkpoint moved the cover
    /// above the end the replay found (reopen: the caller's `visit` has already seen slots now
    /// below the cover); [`Error::Fenced`] when a later opener's fence is already recorded in the
    /// head, so this open lost the race and holds no authority; [`Error::Capacity`] when the epoch
    /// inventory is full and the lane needs a new activation.
    pub async fn open_with_head<F>(
        store: Arc<S>,
        layout: Layout,
        id: LaneId,
        visit: F,
    ) -> Result<(Self, OpenReport)>
    where
        F: FnMut(&SlotView<'_>) -> Result<()>,
    {
        let (head, _) = read_head(&*store, &layout, &id).await?;
        Self::open_at(store, layout, id, head.cover, CoverSource::Head, visit).await
    }

    async fn open_at<F>(
        store: Arc<S>,
        layout: Layout,
        id: LaneId,
        from: u64,
        source: CoverSource,
        mut visit: F,
    ) -> Result<(Self, OpenReport)>
    where
        F: FnMut(&SlotView<'_>) -> Result<()>,
    {
        let (descriptor, version) = read_descriptor(&*store, &layout, &id).await?;
        let mut report = OpenReport {
            from,
            ..OpenReport::default()
        };
        let mut sealed = BTreeMap::new();
        let mut at = from;
        loop {
            at = read_log(&*store, &layout, &id, at, None, |view| {
                report.slots_replayed += 1;
                match view.kind {
                    SlotKind::Fence => report.fences_replayed += 1,
                    SlotKind::Data => {
                        for record in view.records() {
                            report.records_replayed += 1;
                            if record.op == RecordOp::Seal {
                                sealed.insert(record.index, view.position.ordinal);
                            }
                        }
                    }
                }
                visit(view)
            })
            .await?;
            // The first absent position is only the end of the log if it is not a reclaimed hole.
            // The cover is written before any slot below it is removed, so reading it after the
            // probe is sufficient: `at` at or above it was never reclaimed (M03.04's rule).
            let cover = match source {
                CoverSource::Marker => read_cover(&*store, &layout, &id).await?.0,
                CoverSource::Head => read_head(&*store, &layout, &id).await?.0.cover,
            };
            if at < cover {
                return Err(Error::BadCover);
            }
            let token = fresh_token();
            let mut bytes = Vec::new();
            encode_slot(
                &mut bytes,
                &position(&id, at),
                token,
                &SlotBody::Fence {
                    successor_epoch: at,
                },
            )?;
            let request = mutation(
                &request_id("fence", &id.domain, at, &token),
                &slot_key(&layout, &id, at),
                &bytes,
                Precondition::Absent,
            )?;
            match write(&*store, &request, false).await {
                Written::Done(_) => break,
                Written::Occupied => {
                    report.seal_races += 1;
                    if report.seal_races > MAX_SEAL_RACES {
                        return Err(Error::Contended);
                    }
                }
                Written::Refused(error) => return Err(error.into()),
                Written::Unknown => return Err(Error::Unknown { ordinal: at }),
            }
        }
        report.fence_ordinal = at;
        if source == CoverSource::Head {
            let (epoch, generation) = establish(&*store, &layout, &id, at).await?;
            report.head_epoch = epoch;
            report.head_generation = generation;
        }
        let writer = Self {
            store,
            layout,
            id,
            next: at + 1,
            source,
            descriptor,
            version,
            sealed,
            poisoned: false,
            stats: WriterStats::default(),
            scratch: Vec::new(),
            reclaimed_to: from,
        };
        Ok((writer, report))
    }

    pub(crate) fn store_ref(&self) -> &S {
        &self.store
    }

    pub(crate) fn layout_ref(&self) -> &Layout {
        &self.layout
    }

    /// The policy tuple of the lane this writer owns.
    #[must_use]
    pub fn policy(&self) -> &hsearch_format::PolicyTuple {
        &self.descriptor.policy
    }

    /// The lane this writer owns.
    #[must_use]
    pub fn id(&self) -> &LaneId {
        &self.id
    }

    /// The next position an append will use.
    #[must_use]
    pub fn next_ordinal(&self) -> u64 {
        self.next
    }

    /// Whether the writer has stopped and must be reopened from the store.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// The descriptor generation this writer last saw.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.descriptor.generation
    }

    /// Counters of what this writer has acknowledged.
    #[must_use]
    pub fn stats(&self) -> WriterStats {
        self.stats
    }

    /// Members as of the last descriptor read.
    #[must_use]
    pub fn members(&self) -> &[Id] {
        &self.descriptor.members
    }

    /// Whether `index` is sealed in this lane (promoted away or left).
    #[must_use]
    pub fn is_sealed(&self, index: &Id) -> bool {
        self.sealed.contains_key(index)
    }

    /// The position of `index`'s seal record, if sealed.
    #[must_use]
    pub fn sealed_at(&self, index: &Id) -> Option<u64> {
        self.sealed.get(index).copied()
    }

    /// Whether this writer accepts records for `index`: a member and not sealed. A joiner the
    /// writer has not seen yet is looked for once, with a descriptor re-read (2 requests).
    pub async fn admits(&mut self, index: &Id) -> Result<bool> {
        if self.sealed.contains_key(index) {
            return Ok(false);
        }
        if self.descriptor.members.binary_search(index).is_ok() {
            return Ok(true);
        }
        self.refresh_membership().await?;
        Ok(
            self.descriptor.members.binary_search(index).is_ok()
                && !self.sealed.contains_key(index),
        )
    }

    /// Re-read the descriptor (a head and a ranged GET). Joins show up here; a leave only ever
    /// takes effect through a seal this writer wrote itself.
    pub async fn refresh_membership(&mut self) -> Result<()> {
        let (descriptor, version) = read_descriptor(&*self.store, &self.layout, &self.id).await?;
        if descriptor.generation >= self.descriptor.generation {
            self.descriptor = descriptor;
            self.version = version;
        }
        Ok(())
    }

    /// Commit `records` as one slot at the next position, with one conditional create.
    ///
    /// Refused without writing: an empty or oversized batch, a seal record (use
    /// [`LaneWriter::seal_index`]), a record for a non-member or sealed index. **Not poisoned**:
    /// the store certainly refused (`Error::Store`); the same call may be repeated. **Poisoned**:
    /// [`Error::Fenced`] (the position is someone else's) and [`Error::Unknown`] (the reply was lost
    /// and could not be resolved; the slot may exist, and the batch was not acknowledged).
    pub async fn append(&mut self, records: &[RecordSpec<'_>]) -> Result<Ack> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if records.is_empty() || records.len() > MAX_SLOT_RECORDS {
            return Err(Error::Invalid("batch size"));
        }
        // A joiner this writer has not seen is looked for with one descriptor re-read per call,
        // however many unknown indexes the batch names.
        let mut refreshed = false;
        for record in records {
            if record.op == RecordOp::Seal {
                return Err(Error::Invalid("a seal is written by seal_index"));
            }
            if self.sealed.contains_key(&record.index) {
                return Err(Error::Sealed(record.index));
            }
            if self
                .descriptor
                .members
                .binary_search(&record.index)
                .is_err()
            {
                if !refreshed {
                    refreshed = true;
                    self.refresh_membership().await?;
                }
                if self
                    .descriptor
                    .members
                    .binary_search(&record.index)
                    .is_err()
                {
                    return Err(Error::NotMember(record.index));
                }
                if self.sealed.contains_key(&record.index) {
                    return Err(Error::Sealed(record.index));
                }
            }
        }
        self.commit(records).await
    }

    async fn commit(&mut self, records: &[RecordSpec<'_>]) -> Result<Ack> {
        let ordinal = self.next;
        let token: CommitToken = fresh_token();
        self.scratch.clear();
        encode_slot(
            &mut self.scratch,
            &position(&self.id, ordinal),
            token,
            &SlotBody::Data(records),
        )?;
        let request = mutation(
            &request_id("slot", &self.id.domain, ordinal, &token),
            &slot_key(&self.layout, &self.id, ordinal),
            &self.scratch,
            Precondition::Absent,
        )?;
        match write(&*self.store, &request, false).await {
            Written::Done(_) => {
                self.next = ordinal + 1;
                self.stats.slots += 1;
                self.stats.records += records.len() as u64;
                self.stats.bytes += self.scratch.len() as u64;
                let ack = Ack {
                    ordinal,
                    records: records.len(),
                    bytes: self.scratch.len(),
                };
                // A writer that once sent a big slot does not keep that buffer for life.
                if self.scratch.capacity() > SCRATCH_KEEP {
                    self.scratch = Vec::new();
                }
                Ok(ack)
            }
            Written::Occupied => {
                self.poisoned = true;
                Err(Error::Fenced)
            }
            Written::Unknown => {
                self.poisoned = true;
                Err(Error::Unknown { ordinal })
            }
            Written::Refused(error) => Err(error.into()),
        }
    }

    /// Seals still waiting for their index to leave the descriptor: an unfinished promotion.
    /// The cover must not pass one, or a reopen would forget the seal.
    #[must_use]
    pub fn unfinished_promotions(&self) -> Vec<(Id, u64)> {
        self.sealed
            .iter()
            .filter(|(index, _)| self.descriptor.members.binary_search(index).is_ok())
            .map(|(index, at)| (*index, *at))
            .collect()
    }

    /// Remove the **data** slots below `cover`, which the caller's checkpoint has made durable and
    /// covers (every record in them is in published segments). The cover is first raised in its
    /// durable marker, so an opener below it is refused. Fences are never removed: a late create
    /// into a removed fence would succeed and be acknowledged behind the successor's view
    /// (M03.04's `BUG_reclaim_deletes_fences`). Refused while a promotion started in this lane is
    /// unfinished below `cover`. Cost: one HEAD per position, one 93-byte ranged read to tell a
    /// fence from data, and one delete per data slot. Repeatable; `cover` may not pass this
    /// writer's next position; a poisoned writer reclaims nothing.
    pub async fn reclaim_below(&mut self, cover: u64) -> Result<Reclaimed> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if self.source == CoverSource::Head {
            return Err(Error::Invalid(
                "a head-driven lane reclaims to its checkpoint",
            ));
        }
        if cover > self.next {
            return Err(Error::Invalid("cover beyond the log"));
        }
        if self
            .unfinished_promotions()
            .iter()
            .any(|(_, at)| *at < cover)
        {
            return Err(Error::Invalid("cover would pass an unfinished promotion"));
        }
        if cover <= self.reclaimed_to {
            return Ok(Reclaimed::default());
        }
        raise_cover(&*self.store, &self.layout, &self.id, cover).await?;
        self.remove_below(cover).await
    }

    /// Remove the data slots below the cover the structural head already records, keeping every
    /// fence. The head's cover is raised by a checkpoint transition **before** this is called,
    /// which is the rule the marker enforced for [`LaneWriter::reclaim_below`]: nothing is removed
    /// that the durable cover does not already pass. The marker is not touched. Refused while a
    /// promotion started in this lane is unfinished below the cover.
    pub async fn reclaim_to_checkpoint(&mut self) -> Result<Reclaimed> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if self.source == CoverSource::Marker {
            return Err(Error::Invalid(
                "a marker-driven lane reclaims below a marker",
            ));
        }
        let (head, _) = read_head(&*self.store, &self.layout, &self.id).await?;
        let cover = head.cover.min(self.next);
        if self
            .unfinished_promotions()
            .iter()
            .any(|(_, at)| *at < cover)
        {
            return Err(Error::Invalid("cover would pass an unfinished promotion"));
        }
        if cover <= self.reclaimed_to {
            return Ok(Reclaimed::default());
        }
        self.remove_below(cover).await
    }

    async fn remove_below(&mut self, cover: u64) -> Result<Reclaimed> {
        let mut out = Reclaimed::default();
        for ordinal in self.reclaimed_to..cover {
            let k = key(&slot_key(&self.layout, &self.id, ordinal))?;
            let info = match self.store.head(&crate::io::context(), &k).await {
                Ok(info) => info,
                Err(hsearch_object::Error::NotFound) => {
                    out.absent += 1;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if info.length <= FENCE_KIND_AT {
                return Err(Error::Format(hsearch_format::FormatError::Truncated));
            }
            let range =
                hsearch_object::ByteRange::new(0, FENCE_KIND_AT + 1).map_err(Error::from)?;
            let head = self
                .store
                .read_range(&crate::io::context(), &k, &info.version, range)
                .await?;
            if head.as_bytes().get(FENCE_KIND_AT as usize) == Some(&2) {
                out.fences_kept += 1;
                continue;
            }
            match self
                .store
                .remove_version(&crate::io::context(), &k, &info.version)
                .await
            {
                Ok(()) | Err(hsearch_object::Error::NotFound) => out.removed += 1,
                Err(error) => return Err(error.into()),
            }
        }
        self.reclaimed_to = cover;
        Ok(out)
    }

    /// Commit a seal for `index` (one slot, one PUT): from this position no record for it is
    /// accepted by this lane, and its `last_seq` is the sequence the lane last held for it. The
    /// index must be a member and not already sealed.
    pub async fn seal_index(&mut self, index: Id, last_seq: u64) -> Result<Ack> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if self.sealed.contains_key(&index) {
            return Err(Error::Sealed(index));
        }
        if self.sealed.len() >= MAX_SEALED {
            return Err(Error::Capacity);
        }
        if self.descriptor.members.binary_search(&index).is_err() {
            return Err(Error::NotMember(index));
        }
        let record = RecordSpec {
            index,
            seq: last_seq,
            op: RecordOp::Seal,
            payload: hsearch_format::PayloadRef::None,
        };
        let ack = self.commit(std::slice::from_ref(&record)).await?;
        self.sealed.insert(index, ack.ordinal);
        Ok(ack)
    }

    /// Remove a sealed `index` from the descriptor by CAS (one PUT, plus a re-read per lost race).
    /// The seal is what stops its writes; this frees its slot in the group's ceiling.
    pub async fn remove_member(&mut self, index: &Id) -> Result<()> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if !self.sealed.contains_key(index) {
            return Err(Error::Invalid("remove only a sealed index"));
        }
        for _ in 0..8 {
            if self.descriptor.members.binary_search(index).is_err() {
                return Ok(());
            }
            let next = without_member(&self.descriptor, index)?;
            if let Some(version) =
                swap_descriptor(&*self.store, &self.layout, &self.id, &next, &self.version).await?
            {
                self.descriptor = next;
                self.version = version;
                return Ok(());
            }
            self.refresh_membership().await?;
        }
        Err(Error::Contended)
    }
}
