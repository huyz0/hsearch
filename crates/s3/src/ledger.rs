//! Request attribution (M03.07): logical calls and actual attempts are counted apart.
//!
//! A logical call is one adapter operation a caller asked for. An attempt is one request on the
//! wire, including a retry, a conflict or an empty conditional read. Attempts are classified by
//! role (payload, metadata, key), verb and outcome, with the encoded bytes each carried. Nothing
//! here reads a clock or a socket: the adapter records what it did, and the tests inject what a
//! provider SDK would add.

use std::collections::BTreeMap;

/// What an attempt carried for the system.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Role {
    /// Object payload bytes.
    Payload,
    /// Commit, catalog and other metadata objects.
    Metadata,
    /// Key-management operations (wrap, unwrap, rotate).
    Key,
}

/// The HTTP verb of an attempt as the provider bills it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Verb {
    /// Object or bucket read.
    Get,
    /// Object write; also bucket configuration.
    Put,
    /// Object metadata read.
    Head,
    /// Version listing.
    List,
    /// Object delete.
    Delete,
    /// Multipart control request, billed as a write.
    Post,
}

/// How an attempt ended, from the provider's response alone.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Outcome {
    /// A 2xx response.
    Ok,
    /// A write refused by a failed precondition.
    Conflict,
    /// A read that found nothing (404) or failed a read precondition (304, 412).
    EmptyRead,
    /// A 429 or 503: the provider asking the client to slow down.
    Throttled,
    /// Any other 5xx.
    ServerError,
    /// Any other 4xx or unexpected status.
    ClientError,
    /// No response: the connection failed or timed out.
    Transport,
}

impl Outcome {
    /// Classify a response. `None` is no response at all.
    #[must_use]
    pub fn classify(verb: Verb, status: Option<u16>) -> Self {
        let Some(status) = status else {
            return Outcome::Transport;
        };
        let read = matches!(verb, Verb::Get | Verb::Head);
        match status {
            200..=299 => Outcome::Ok,
            404 | 304 | 412 if read => Outcome::EmptyRead,
            // Deleting what is not there found nothing, as a read does.
            404 if verb == Verb::Delete => Outcome::EmptyRead,
            404 | 409 | 412 => Outcome::Conflict,
            429 | 503 => Outcome::Throttled,
            500..=599 => Outcome::ServerError,
            _ => Outcome::ClientError,
        }
    }
}

/// Counts and encoded bytes for one (role, verb, outcome) cell.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cell {
    /// Attempts on the wire.
    pub attempts: u64,
    /// Encoded request bytes sent, headers included.
    pub request_bytes: u64,
    /// Encoded response bytes received, headers included.
    pub response_bytes: u64,
}

/// Logical calls and attempts, kept independently.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Ledger {
    logical: BTreeMap<Role, u64>,
    cells: BTreeMap<(Role, Verb, Outcome), Cell>,
    retries: u64,
}

impl Ledger {
    /// Record one logical call. A call refused before the wire has no attempt.
    pub fn logical(&mut self, role: Role) {
        *self.logical.entry(role).or_default() += 1;
    }

    /// Record one attempt. `retry` is true when it repeats an earlier attempt of the same call.
    pub fn attempt(
        &mut self,
        role: Role,
        verb: Verb,
        outcome: Outcome,
        request_bytes: u64,
        response_bytes: u64,
        retry: bool,
    ) {
        let cell = self.cells.entry((role, verb, outcome)).or_default();
        cell.attempts += 1;
        cell.request_bytes = cell.request_bytes.saturating_add(request_bytes);
        cell.response_bytes = cell.response_bytes.saturating_add(response_bytes);
        self.retries += u64::from(retry);
    }

    /// Logical calls of a role.
    #[must_use]
    pub fn logical_calls(&self, role: Role) -> u64 {
        self.logical.get(&role).copied().unwrap_or(0)
    }

    /// Attempts of a role, all verbs and outcomes.
    #[must_use]
    pub fn attempts(&self, role: Role) -> u64 {
        self.cells
            .iter()
            .filter(|((r, _, _), _)| *r == role)
            .map(|(_, c)| c.attempts)
            .sum()
    }

    /// Attempts of a verb across roles; these are what a provider bills.
    #[must_use]
    pub fn attempts_by_verb(&self, verb: Verb) -> u64 {
        self.cells
            .iter()
            .filter(|((_, v, _), _)| *v == verb)
            .map(|(_, c)| c.attempts)
            .sum()
    }

    /// Attempts with a given outcome, across roles and verbs.
    #[must_use]
    pub fn attempts_with(&self, outcome: Outcome) -> u64 {
        self.cells
            .iter()
            .filter(|((_, _, o), _)| *o == outcome)
            .map(|(_, c)| c.attempts)
            .sum()
    }

    /// Attempts flagged as repeats of an earlier attempt.
    #[must_use]
    pub fn retries(&self) -> u64 {
        self.retries
    }

    /// One cell, zero when nothing was recorded.
    #[must_use]
    pub fn cell(&self, role: Role, verb: Verb, outcome: Outcome) -> Cell {
        self.cells
            .get(&(role, verb, outcome))
            .copied()
            .unwrap_or_default()
    }

    /// Attempts per logical call for a role as a `(attempts, calls)` pair; `None` before any call.
    /// Kept as a pair so no rounding hides a small amplification.
    #[must_use]
    pub fn amplification(&self, role: Role) -> Option<(u64, u64)> {
        match self.logical_calls(role) {
            0 => None,
            calls => Some((self.attempts(role), calls)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_separates_conflicts_empty_reads_and_throttling() {
        use Outcome as O;
        use Verb as V;
        let cases = [
            (V::Put, Some(200), O::Ok),
            (V::Put, Some(412), O::Conflict),
            (V::Put, Some(409), O::Conflict),
            (V::Get, Some(404), O::EmptyRead),
            (V::Head, Some(412), O::EmptyRead),
            (V::Get, Some(304), O::EmptyRead),
            (V::Put, Some(404), O::Conflict),
            (V::Delete, Some(404), O::EmptyRead),
            (V::Get, Some(503), O::Throttled),
            (V::Put, Some(429), O::Throttled),
            (V::Get, Some(500), O::ServerError),
            (V::Get, Some(403), O::ClientError),
            (V::Put, None, O::Transport),
        ];
        for (verb, status, want) in cases {
            assert_eq!(Outcome::classify(verb, status), want, "{verb:?} {status:?}");
        }
    }

    #[test]
    fn logical_calls_and_attempts_are_independent() {
        let mut ledger = Ledger::default();
        // One logical write that took three attempts: throttled, transport failure, then ok.
        ledger.logical(Role::Metadata);
        ledger.attempt(
            Role::Metadata,
            Verb::Put,
            Outcome::Throttled,
            100,
            20,
            false,
        );
        ledger.attempt(Role::Metadata, Verb::Put, Outcome::Transport, 100, 0, true);
        ledger.attempt(Role::Metadata, Verb::Put, Outcome::Ok, 100, 30, true);
        // A call refused before the wire: logical, no attempt.
        ledger.logical(Role::Payload);
        assert_eq!(ledger.logical_calls(Role::Metadata), 1);
        assert_eq!(ledger.attempts(Role::Metadata), 3);
        assert_eq!(ledger.retries(), 2);
        assert_eq!(ledger.amplification(Role::Metadata), Some((3, 1)));
        assert_eq!(ledger.amplification(Role::Payload), Some((0, 1)));
        assert_eq!(ledger.amplification(Role::Key), None);
        assert_eq!(ledger.attempts_by_verb(Verb::Put), 3);
        assert_eq!(ledger.attempts_with(Outcome::Throttled), 1);
        let ok = ledger.cell(Role::Metadata, Verb::Put, Outcome::Ok);
        assert_eq!(
            (ok.attempts, ok.request_bytes, ok.response_bytes),
            (1, 100, 30)
        );
        assert_eq!(
            ledger.cell(Role::Key, Verb::Get, Outcome::Ok),
            Cell::default()
        );
    }

    #[test]
    fn conflicts_and_empty_conditional_reads_are_attempts_not_free() {
        let mut ledger = Ledger::default();
        ledger.logical(Role::Metadata);
        ledger.attempt(Role::Metadata, Verb::Put, Outcome::Conflict, 50, 10, false);
        ledger.attempt(Role::Metadata, Verb::List, Outcome::Ok, 40, 900, false);
        ledger.attempt(
            Role::Metadata,
            Verb::Head,
            Outcome::EmptyRead,
            40,
            10,
            false,
        );
        assert_eq!(ledger.attempts(Role::Metadata), 3);
        assert_eq!(ledger.attempts_with(Outcome::Conflict), 1);
        assert_eq!(ledger.attempts_with(Outcome::EmptyRead), 1);
        assert_eq!(ledger.retries(), 0);
    }

    #[test]
    fn byte_totals_saturate_instead_of_wrapping() {
        let mut ledger = Ledger::default();
        ledger.attempt(Role::Payload, Verb::Put, Outcome::Ok, u64::MAX, 1, false);
        ledger.attempt(Role::Payload, Verb::Put, Outcome::Ok, 5, 1, false);
        let cell = ledger.cell(Role::Payload, Verb::Put, Outcome::Ok);
        assert_eq!(
            (cell.attempts, cell.request_bytes, cell.response_bytes),
            (2, u64::MAX, 2)
        );
    }
}
