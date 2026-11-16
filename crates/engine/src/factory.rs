//! The one public boundary over the pinned Lucene revision (M04.01).
//!
//! Callers ask the factory for a [`ShardEngine`] and for what the engine
//! supports; they never reach a Lucene type. Every capability has exactly one
//! inventory entry, and an unsupported one is refused with
//! [`Error::Unsupported`] instead of being ignored.
use crate::{
    Checkpoint, Engine, EngineDirectory, Error, Query, Receipt, Result, Schema, Snapshot, Sort,
    adapter::PreparedBatch,
};
use lucene_store::Directory;

/// A selected core-v1 capability, or a named one the engine does not offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    /// Lookup by logical identity.
    QueryId,
    /// Match every live document.
    QueryAll,
    /// Exact term.
    QueryTerm,
    /// OR of exact terms.
    QueryTerms,
    /// Inclusive integer or date range.
    QueryRange,
    /// Analyzed text match.
    QueryMatch,
    /// Boolean combination.
    QueryBool,
    /// A query shape outside the selected IR.
    UnsupportedQuery,
    /// Local BM25 score order.
    SortScore,
    /// Single-valued declared field order.
    SortField,
    /// Terms and metric aggregations over a snapshot.
    Aggregations,
    /// Whole-document put, create-only put and delete with caller versions.
    ResolvedMutations,
    /// Native commit returning caller-held retry state.
    NativeCommit,
    /// Segments materialized into the local directory before reading.
    MaterializedInput,
    /// Segments read by range through the page cache, never fully resident.
    RemoteRangeInput,
    /// Scripted updates evaluated by the engine.
    ScriptedUpdate,
    /// More than one live writer per directory.
    MultiWriter,
}

impl Capability {
    /// Position in [`Capability::ALL`] and the inventory. Exhaustive: a new
    /// variant does not compile until it has a slot.
    pub const fn index(self) -> usize {
        match self {
            Self::QueryId => 0,
            Self::QueryAll => 1,
            Self::QueryTerm => 2,
            Self::QueryTerms => 3,
            Self::QueryRange => 4,
            Self::QueryMatch => 5,
            Self::QueryBool => 6,
            Self::UnsupportedQuery => 7,
            Self::SortScore => 8,
            Self::SortField => 9,
            Self::Aggregations => 10,
            Self::ResolvedMutations => 11,
            Self::NativeCommit => 12,
            Self::MaterializedInput => 13,
            Self::RemoteRangeInput => 14,
            Self::ScriptedUpdate => 15,
            Self::MultiWriter => 16,
        }
    }
    /// Every capability, so the inventory can be checked for completeness.
    pub const ALL: &'static [Capability] = &[
        Self::QueryId,
        Self::QueryAll,
        Self::QueryTerm,
        Self::QueryTerms,
        Self::QueryRange,
        Self::QueryMatch,
        Self::QueryBool,
        Self::UnsupportedQuery,
        Self::SortScore,
        Self::SortField,
        Self::Aggregations,
        Self::ResolvedMutations,
        Self::NativeCommit,
        Self::MaterializedInput,
        Self::RemoteRangeInput,
        Self::ScriptedUpdate,
        Self::MultiWriter,
    ];
}

/// Whether a capability is offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// Offered and tested.
    Supported,
    /// Not offered; the reason names the owner or decision.
    Unsupported {
        /// Why, and which task or decision owns it.
        reason: &'static str,
    },
}

/// One inventory row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feature {
    /// The capability.
    pub capability: Capability,
    /// Its status.
    pub support: Support,
}

const fn yes(capability: Capability) -> Feature {
    Feature {
        capability,
        support: Support::Supported,
    }
}
const fn no(capability: Capability, reason: &'static str) -> Feature {
    Feature {
        capability,
        support: Support::Unsupported { reason },
    }
}

static INVENTORY: &[Feature] = &[
    yes(Capability::QueryId),
    yes(Capability::QueryAll),
    yes(Capability::QueryTerm),
    yes(Capability::QueryTerms),
    yes(Capability::QueryRange),
    yes(Capability::QueryMatch),
    yes(Capability::QueryBool),
    no(
        Capability::UnsupportedQuery,
        "outside the selected core-v1 query IR; no unknown-option fallback",
    ),
    yes(Capability::SortScore),
    yes(Capability::SortField),
    yes(Capability::Aggregations),
    yes(Capability::ResolvedMutations),
    yes(Capability::NativeCommit),
    yes(Capability::MaterializedInput),
    yes(Capability::RemoteRangeInput),
    no(
        Capability::ScriptedUpdate,
        "scripts stay out of the engine (A11); updates are resolved before the engine",
    ),
    no(
        Capability::MultiWriter,
        "one writer per directory; authority is the commit lane's, not the engine's",
    ),
];

/// How segment bytes reach the reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterMode {
    /// Materialized local directory.
    Materialized,
    /// Range-native remote input (not yet offered).
    RemoteRange,
}

/// The capability a query shape needs. Exhaustive: a new variant fails to
/// compile until it is classified.
pub fn capability_of_query(query: &Query<'_>) -> Capability {
    match query {
        Query::Id(_) => Capability::QueryId,
        Query::All => Capability::QueryAll,
        Query::Term { .. } => Capability::QueryTerm,
        Query::Terms { .. } => Capability::QueryTerms,
        Query::Range { .. } => Capability::QueryRange,
        Query::Match { .. } => Capability::QueryMatch,
        Query::Bool { .. } => Capability::QueryBool,
        Query::Unsupported => Capability::UnsupportedQuery,
    }
}

/// The capability a sort needs. Exhaustive like [`capability_of_query`].
pub fn capability_of_sort(sort: &Sort<'_>) -> Capability {
    match sort {
        Sort::Score => Capability::SortScore,
        Sort::Field { .. } => Capability::SortField,
    }
}

/// Opens shard engines and answers what they support.
#[derive(Clone, Copy, Debug, Default)]
pub struct EngineFactory;

impl EngineFactory {
    /// The factory for the pinned revision.
    pub fn new() -> Self {
        Self
    }
    /// The exact upstream revision this build is pinned to.
    pub fn revision(&self) -> &'static str {
        crate::ENGINE_REVISION
    }
    /// The full feature inventory.
    pub fn inventory(&self) -> &'static [Feature] {
        INVENTORY
    }
    /// Status of one capability.
    pub fn support(&self, capability: Capability) -> Support {
        INVENTORY
            .iter()
            .find(|f| f.capability == capability)
            .map_or(
                Support::Unsupported {
                    reason: "not in the inventory",
                },
                |f| f.support,
            )
    }
    /// `Ok` when offered, otherwise [`Error::Unsupported`].
    pub fn require(&self, capability: Capability) -> Result<()> {
        match self.support(capability) {
            Support::Supported => Ok(()),
            Support::Unsupported { .. } => Err(Error::Unsupported),
        }
    }
    /// Open a shard engine. A mode that is not offered is refused before the
    /// directory is touched.
    pub fn open<'d, D: Directory>(
        &self,
        directory: &'d EngineDirectory<D>,
        schema: Schema,
        checkpoint: Option<Checkpoint>,
        mode: WriterMode,
    ) -> Result<Engine<'d>> {
        self.require(match mode {
            WriterMode::Materialized => Capability::MaterializedInput,
            WriterMode::RemoteRange => Capability::RemoteRangeInput,
        })?;
        Engine::open(directory, schema, checkpoint)
    }
}

/// The lifecycle every shard engine offers, with no Lucene type in it.
pub trait ShardEngine {
    /// Apply a prepared, caller-resolved batch.
    fn apply(&self, batch: PreparedBatch) -> Result<Vec<Receipt>>;
    /// Publish a frozen reader.
    fn refresh(&self) -> Result<Snapshot>;
    /// Commit native files and return retry state.
    fn commit(&self) -> Result<Checkpoint>;
    /// Bounded explicit maintenance.
    fn merge(&self) -> Result<()>;
    /// Close without committing, consuming the engine.
    fn close_boxed(self: Box<Self>) -> Result<()>;
}

impl ShardEngine for Engine<'_> {
    fn apply(&self, batch: PreparedBatch) -> Result<Vec<Receipt>> {
        Engine::apply(self, batch)
    }
    fn refresh(&self) -> Result<Snapshot> {
        Engine::refresh(self)
    }
    fn commit(&self) -> Result<Checkpoint> {
        Engine::commit(self)
    }
    fn merge(&self) -> Result<()> {
        Engine::merge(self)
    }
    fn close_boxed(self: Box<Self>) -> Result<()> {
        Engine::close(*self)
    }
}
