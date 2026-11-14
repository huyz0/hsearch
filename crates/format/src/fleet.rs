//! Fleet format-version record (PR-10, PR-52): the durable, conditionally-written statement of
//! which formats nodes may write, and the rule by which a node that cannot honour it refuses.
use crate::codec::{Reader, Writer};
use crate::envelope::{Kind, open};
use crate::{CommitToken, FormatError, Result};

/// The record stored at the fleet key and replaced only by conditional write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FleetRecord {
    /// Starts at 1; every replacement is exactly one higher.
    pub generation: u64,
    /// Highest format version any node may write. Raised by the operator once every node can read it.
    pub write_format: u16,
    /// Oldest format version a node must still read; nodes that cannot read it must not join.
    pub min_read_format: u16,
    /// Mandatory capabilities every node must implement to serve the fleet.
    pub required_caps: u64,
    /// Unique per commit so a stale writer's identical bytes still differ (M03.11).
    pub token: CommitToken,
}

/// What one node binary supports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeSupport {
    /// Oldest format version this node still reads.
    pub min_read_format: u16,
    /// Newest format version this node reads.
    pub max_read_format: u16,
    /// Newest format version this node writes.
    pub max_write_format: u16,
    /// Mandatory capability bits this node implements.
    pub caps: u64,
}

/// Why a node must not serve the fleet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// Some node may write a format this node cannot read.
    FleetWritesNewerThanNodeReads,
    /// The node would write a format below the fleet's oldest readable one (or format 0).
    NodeWritesBelowReadableMinimum,
    /// The fleet still holds objects of a format older than this node can read.
    NodeCannotReadOldestFormat,
    /// The fleet requires capability bits this node lacks; the mask is the missing bits.
    UnknownMandatoryCapability(u64),
}

impl FleetRecord {
    /// Append the encoded record to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.generation == 0
            || self.min_read_format == 0
            || self.min_read_format > self.write_format
        {
            return Err(FormatError::Malformed("fleet versions"));
        }
        let mut w = Writer::begin(out, Kind::Fleet, 1, 0);
        w.u64(self.generation);
        w.u16(self.write_format);
        w.u16(self.min_read_format);
        w.u64(self.required_caps);
        w.bytes(self.token.as_bytes());
        w.finish()
    }

    /// Decode and validate one whole object.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::Fleet)?;
        let mut r = Reader::new(body);
        let generation = r.u64()?;
        let write_format = r.u16()?;
        let min_read_format = r.u16()?;
        let required_caps = r.u64()?;
        let token = CommitToken::new(r.array()?)?;
        r.finish()?;
        if generation == 0 || min_read_format == 0 || min_read_format > write_format {
            return Err(FormatError::Malformed("fleet versions"));
        }
        Ok(Self {
            generation,
            write_format,
            min_read_format,
            required_caps,
            token,
        })
    }

    /// May this node serve the fleet? Returns the format version it may write.
    pub fn admit(&self, node: &NodeSupport) -> std::result::Result<u16, Refusal> {
        let missing = self.required_caps & !node.caps;
        if missing != 0 {
            return Err(Refusal::UnknownMandatoryCapability(missing));
        }
        if node.max_read_format < self.write_format {
            return Err(Refusal::FleetWritesNewerThanNodeReads);
        }
        if node.min_read_format > self.min_read_format {
            return Err(Refusal::NodeCannotReadOldestFormat);
        }
        if node.max_write_format < self.min_read_format {
            return Err(Refusal::NodeWritesBelowReadableMinimum);
        }
        Ok(self.write_format.min(node.max_write_format))
    }

    /// May `next` replace `self` by conditional write? Generation steps by one, the token is
    /// new, the formats stay ordered, and `write_format` and the required capabilities never fall.
    pub fn validate_successor(&self, next: &Self) -> Result<()> {
        if self.generation.checked_add(1) != Some(next.generation) {
            return Err(FormatError::Malformed("generation must step by one"));
        }
        if self.token == next.token {
            return Err(FormatError::Malformed("commit token reused"));
        }
        // Raising write_format is the point of no return: nodes then write objects an older
        // binary cannot read, so it may never fall, and capabilities may only be added.
        if next.write_format < self.write_format {
            return Err(FormatError::Malformed("write format cannot fall"));
        }
        if next.required_caps & self.required_caps != self.required_caps {
            return Err(FormatError::Malformed(
                "required capabilities cannot be removed",
            ));
        }
        if next.min_read_format < self.min_read_format {
            return Err(FormatError::Malformed(
                "minimum readable format cannot fall",
            ));
        }
        Ok(())
    }
}
