//! An index's base pointer (M06.24): the one conditional object that says which base a shard
//! recovers from, how much of the log that base covers, and where on its lane the log above it
//! starts.
//!
//! On a lane shared by several indexes the lane's structural head cannot name one checkpoint, so
//! each shard keeps its own pointer. It changes only when a base is published (never per append),
//! and [`BasePointer::validate_successor`] is the predicate a writer checks against the version it
//! read before it replaces the object, so a paused writer's older view can never lower it.
use crate::codec::{Reader, Writer};
use crate::envelope::{Kind, open};
use crate::{CommitToken, FormatError, Id, Result};

/// Which base a shard recovers from and where on the lane the log above it starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BasePointer {
    /// The index (also in the key).
    pub index: Id,
    /// The shard (also in the key).
    pub shard: Id,
    /// The shard incarnation (also in the key).
    pub incarnation: u64,
    /// The manifest root of the base, never all zero.
    pub manifest: Id,
    /// That root's publication generation.
    pub manifest_generation: u64,
    /// The index's own sequence: every record below it is in the base.
    pub covered: u64,
    /// The lane the log above the base is on: domain, incarnation, activation and lane number.
    pub lane_domain: Id,
    /// The lane's incarnation.
    pub lane_incarnation: u64,
    /// The lane's activation.
    pub lane_activation: u64,
    /// The lane number.
    pub lane_number: u32,
    /// The lowest lane ordinal that can hold a record of this index above `covered`, or the lane's
    /// tail when none can: replay starts here, never below the lane head's cover.
    pub lane_cover: u64,
    /// Starts at 1, one higher per write.
    pub sequence: u64,
    /// Unique per commit.
    pub token: CommitToken,
}

/// Why a pointer may not replace the one the writer read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The sequence is not exactly one higher.
    Sequence,
    /// The pointer names another index, shard or incarnation.
    Identity,
    /// The covered sequence would fall.
    Covered,
    /// The lane cover would fall on the same lane.
    LaneCover,
    /// The manifest generation would fall.
    Generation,
    /// The lane's incarnation or activation would fall.
    LaneRegressed,
    /// The lane cover is below the lane head's cover, which a reclaim may already have passed.
    BelowHeadCover,
}

impl BasePointer {
    /// Append the encoded pointer to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut w = Writer::begin(out, Kind::BasePointer, 1, 0);
        w.bytes(&self.index);
        w.bytes(&self.shard);
        w.u64(self.incarnation);
        w.bytes(&self.manifest);
        w.u64(self.manifest_generation);
        w.u64(self.covered);
        w.bytes(&self.lane_domain);
        w.u64(self.lane_incarnation);
        w.u64(self.lane_activation);
        w.u32(self.lane_number);
        w.u64(self.lane_cover);
        w.u64(self.sequence);
        w.bytes(self.token.as_bytes());
        w.finish()
    }

    /// Decode and validate one whole object.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::BasePointer)?;
        let mut r = Reader::new(body);
        let p = Self {
            index: r.id()?,
            shard: r.id()?,
            incarnation: r.u64()?,
            manifest: r.id()?,
            manifest_generation: r.u64()?,
            covered: r.u64()?,
            lane_domain: r.id()?,
            lane_incarnation: r.u64()?,
            lane_activation: r.u64()?,
            lane_number: r.u32()?,
            lane_cover: r.u64()?,
            sequence: r.u64()?,
            token: CommitToken::new(r.array()?)?,
        };
        r.finish()?;
        if p.incarnation == 0
            || p.lane_incarnation == 0
            || p.lane_activation == 0
            || p.sequence == 0
        {
            return Err(FormatError::Malformed("base pointer counters"));
        }
        if p.manifest == [0; 16] {
            return Err(FormatError::Malformed("base pointer manifest"));
        }
        Ok(p)
    }

    /// Decode and require the index, shard and incarnation the key named.
    pub fn decode_for(bytes: &[u8], index: &Id, shard: &Id, incarnation: u64) -> Result<Self> {
        let p = Self::decode(bytes)?;
        if &p.index != index || &p.shard != shard || p.incarnation != incarnation {
            return Err(FormatError::Mismatch("base pointer identity"));
        }
        Ok(p)
    }

    /// Whether `self` may replace `current`, the pointer the writer read, on a lane whose head
    /// covers slots below `head_cover`. A first pointer (no `current`) starts at sequence 1.
    ///
    /// # Errors
    /// The [`Refusal`] naming the rule broken.
    pub fn validate_successor(
        &self,
        current: Option<&Self>,
        head_cover: u64,
    ) -> core::result::Result<(), Refusal> {
        let same_lane = |a: &Self, b: &Self| {
            a.lane_domain == b.lane_domain
                && a.lane_incarnation == b.lane_incarnation
                && a.lane_activation == b.lane_activation
                && a.lane_number == b.lane_number
        };
        match current {
            None if self.sequence != 1 => return Err(Refusal::Sequence),
            None => {}
            Some(old) => {
                if self.index != old.index
                    || self.shard != old.shard
                    || self.incarnation != old.incarnation
                {
                    return Err(Refusal::Identity);
                }
                if self.sequence != old.sequence + 1 {
                    return Err(Refusal::Sequence);
                }
                if self.covered < old.covered {
                    return Err(Refusal::Covered);
                }
                if self.manifest_generation < old.manifest_generation {
                    return Err(Refusal::Generation);
                }
                if (self.lane_incarnation, self.lane_activation)
                    < (old.lane_incarnation, old.lane_activation)
                {
                    return Err(Refusal::LaneRegressed);
                }
                if same_lane(self, old) && self.lane_cover < old.lane_cover {
                    return Err(Refusal::LaneCover);
                }
            }
        }
        if self.lane_cover < head_cover {
            return Err(Refusal::BelowHeadCover);
        }
        Ok(())
    }
}
