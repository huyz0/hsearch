//! The log a serving writer commits through: the direct committer of one lane, or a handle to a
//! lane shared by the hosts of several indexes ([`crate::shared`]).
use hsearch_commit::{Committer, DependencyView, External, Outcome, Request};
use hsearch_format::Id;
use hsearch_lane::{Error as LaneError, Reclaimed, block_on};
use hsearch_object::MetadataStore;

/// What a serving writer needs of its lane. Calls block until the outcomes are known.
pub trait Log {
    /// Commit `requests` in order, one outcome per request, same order.
    fn commit(&mut self, requests: Vec<Request>) -> Vec<Outcome>;
    /// Commit one put whose body is already in its own object.
    fn commit_external(&mut self, request: Request, external: External) -> Outcome;
    /// Seal `index` at `last_seq` and remove it from the lane's members. Repeating a finished
    /// leave is a no-op.
    ///
    /// # Errors
    /// [`LaneError`] when the lane refused or failed; the call may be repeated.
    fn leave(&mut self, index: Id, last_seq: u64) -> Result<(), LaneError>;
    /// Remove the slots below the head's cover. The cover is the lane's, moved by the pinned set;
    /// this reads it once and removes what lies below it that earlier calls did not.
    ///
    /// # Errors
    /// [`LaneError`] when the head or a slot could not be handled.
    fn reclaim(&mut self) -> Result<Reclaimed, LaneError>;
    /// Whether the lane writer failed in a way that refuses every later call.
    fn is_poisoned(&self) -> bool;
}

impl<S: MetadataStore, D: DependencyView> Log for Committer<S, D> {
    fn commit(&mut self, requests: Vec<Request>) -> Vec<Outcome> {
        block_on(Committer::commit(self, requests))
    }
    fn commit_external(&mut self, request: Request, external: External) -> Outcome {
        block_on(Committer::commit_external(self, request, external))
    }
    fn leave(&mut self, index: Id, last_seq: u64) -> Result<(), LaneError> {
        block_on(Committer::leave(self, index, last_seq))
    }
    fn reclaim(&mut self) -> Result<Reclaimed, LaneError> {
        block_on(Committer::reclaim(self))
    }
    fn is_poisoned(&self) -> bool {
        Committer::is_poisoned(self)
    }
}
