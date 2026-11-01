//! Shared checkpoint types. Persisted encodings and distributed protocols are not defined here.

/// An ordered logical shard coordinate. Zero denotes the empty prefix.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Sequence(u64);

impl Sequence {
    /// Construct a coordinate already validated by the owning protocol.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric coordinate.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advance without wrapping into an older prefix.
    #[must_use]
    pub fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// A checkpoint relationship that would expose speculative or unavailable data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidProgress {
    /// Application cannot run ahead of committed operations.
    AppliedAheadOfCommit,
    /// Search cannot observe operations that have not been applied.
    SearchableAheadOfApplied,
    /// An archive cannot certify uncommitted operations.
    ArchivedAheadOfCommit,
}

/// Coordinates for one activation and logical shard, supplied by its owner.
///
/// Archival and application are independent consumers of the committed prefix.
/// These comparisons do not establish identity, authority, durability or freshness.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Progress {
    committed: Sequence,
    applied: Sequence,
    searchable: Sequence,
    archived: Sequence,
}

impl Progress {
    /// Validate checkpoint ordering within a single activation and shard.
    pub fn try_new(
        committed: Sequence,
        applied: Sequence,
        searchable: Sequence,
        archived: Sequence,
    ) -> Result<Self, InvalidProgress> {
        if applied > committed {
            return Err(InvalidProgress::AppliedAheadOfCommit);
        }
        if searchable > applied {
            return Err(InvalidProgress::SearchableAheadOfApplied);
        }
        if archived > committed {
            return Err(InvalidProgress::ArchivedAheadOfCommit);
        }
        Ok(Self {
            committed,
            applied,
            searchable,
            archived,
        })
    }

    /// The accepted committed prefix.
    #[must_use]
    pub const fn committed(self) -> Sequence {
        self.committed
    }

    /// The engine's applied prefix.
    #[must_use]
    pub const fn applied(self) -> Sequence {
        self.applied
    }

    /// The pinned reader's searchable prefix.
    #[must_use]
    pub const fn searchable(self) -> Sequence {
        self.searchable
    }

    /// The prefix recoverable from the published archive checkpoint.
    #[must_use]
    pub const fn archived(self) -> Sequence {
        self.archived
    }
}
