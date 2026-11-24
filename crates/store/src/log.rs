//! The log a serving writer commits through: the direct committer of one lane, or a handle to a
//! lane shared by the hosts of several indexes ([`crate::shared`]).
use hsearch_commit::{Committer, DependencyView, External, Outcome, Request};
use hsearch_lane::block_on;
use hsearch_object::MetadataStore;

/// What a serving writer needs of its lane. Calls block until the outcomes are known.
pub trait Log {
    /// Commit `requests` in order, one outcome per request, same order.
    fn commit(&mut self, requests: Vec<Request>) -> Vec<Outcome>;
    /// Commit one put whose body is already in its own object.
    fn commit_external(&mut self, request: Request, external: External) -> Outcome;
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
    fn is_poisoned(&self) -> bool {
        Committer::is_poisoned(self)
    }
}
