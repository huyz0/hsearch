//! Seal on open: before a node writes a lane it has not written, it reads the tail above the head's
//! cover and finds the indexes that have records no published base covers. Each of them is
//! recovered and publishes a base before the open completes, and until then pins the lane's cover.
//!
//! Cost: two requests per slot of the tail (the slot's HEAD and read), one descriptor read, one
//! pointer read per index that wrote in the tail and, for one that left the lane, one route read.
//! Nothing here scales with the lane's members.
use std::collections::BTreeMap;

use hsearch_format::{Id, SlotKind};
use hsearch_lane::{
    Error as LaneError, LaneId, Route, block_on, read_descriptor, read_head, read_log, route,
};
use hsearch_object::{LargeObjectWriter, MetadataStore};

use crate::pointer;
use crate::publish::Publisher;

/// Why a tail could not be scanned.
#[derive(Debug)]
pub enum Error {
    /// The tail holds more records than the bound: publish bases sooner.
    Limit,
    /// The lane could not be read.
    Lane(String),
    /// A pointer could not be read.
    Pointer(String),
}

/// What an index wrote in the tail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Standing {
    /// A member of the lane: pinned until it publishes a base.
    Member,
    /// Promoted to another lane: pinned on this lane until its new-lane base covers its seal.
    Promoted,
}

/// An index with a record above its base.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TailIndex {
    /// The index.
    pub index: Id,
    /// The lowest slot holding a record the index's base does not cover.
    pub first: u64,
    /// The sequence its base covers (0 before the first base).
    pub covered: u64,
    /// Member or promoted.
    pub standing: Standing,
}

/// The tail above a lane head's cover.
#[derive(Debug, Eq, PartialEq)]
pub struct Tail {
    /// The first absent slot.
    pub end: u64,
    /// Slots read.
    pub slots: u64,
    /// Indexes with an uncovered record, in index order. A deleted index is not here.
    pub indexes: Vec<TailIndex>,
}

/// Scan `lane` from its head's cover and find the pinned set a node must start with.
/// `shard_of` maps an index to the shard and incarnation its pointer lives under; `max_records`
/// bounds the records held in memory.
pub fn scan<S, F>(
    publisher: &Publisher<S>,
    lane: &LaneId,
    max_records: usize,
    shard_of: F,
) -> Result<Tail, Error>
where
    S: MetadataStore + LargeObjectWriter + 'static,
    F: Fn(&Id) -> (Id, u64),
{
    let store = publisher.store();
    let layout = publisher.layout();
    let lane_err = |e: LaneError| Error::Lane(format!("{e:?}"));
    let (head, _) = block_on(read_head(store, layout, lane)).map_err(lane_err)?;
    let mut seen: BTreeMap<Id, Vec<(u64, u64)>> = BTreeMap::new();
    let mut held = 0usize;
    let mut slots = 0u64;
    let end = block_on(read_log(store, layout, lane, head.cover, None, |view| {
        slots += 1;
        if view.kind != SlotKind::Data {
            return Ok(());
        }
        for record in view.records() {
            held += 1;
            if held > max_records {
                return Err(LaneError::Invalid("tail over its bound"));
            }
            let at = view.position.ordinal;
            // A seal names the last sequence the index wrote, so it counts at that sequence.
            let seq = record.seq;
            seen.entry(record.index).or_default().push((seq, at));
        }
        Ok(())
    }))
    .map_err(|e| match e {
        LaneError::Invalid("tail over its bound") => Error::Limit,
        other => lane_err(other),
    })?;
    let (descriptor, _) = block_on(read_descriptor(store, layout, lane)).map_err(lane_err)?;
    let io = publisher.io();
    let mut indexes = Vec::new();
    for (index, records) in seen {
        let (shard, incarnation) = shard_of(&index);
        let covered = pointer::read(&io, layout, &index, &shard, incarnation)
            .map_err(|e| Error::Pointer(format!("{e:?}")))?
            .map_or(0, |(p, _)| p.covered);
        let Some(&(_, first)) = records.iter().find(|(seq, _)| *seq >= covered) else {
            continue;
        };
        let standing = if descriptor.members.binary_search(&index).is_ok() {
            Standing::Member
        } else {
            match block_on(route(store, layout, lane, &index)).map_err(lane_err)? {
                Route::Promoted { .. } => Standing::Promoted,
                // Deleted: its seal and removed membership say no one reads these records.
                Route::Shared(_) | Route::Unrouted => continue,
            }
        };
        indexes.push(TailIndex {
            index,
            first,
            covered,
            standing,
        });
    }
    Ok(Tail {
        end,
        slots,
        indexes,
    })
}
