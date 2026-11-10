//! Exact bounded record values. Capture/oracle digest verification is upstream.
use std::collections::BTreeSet;
use std::fmt;

/// Logical operation ceiling.
pub const MAX_OPERATIONS: usize = 256;
/// I/O attempt-record ceiling, including pre-send refusals.
pub const MAX_ATTEMPTS: usize = 512;
/// Interned value ceiling.
pub const MAX_ATOMS: usize = 1024;
/// Framing-inclusive dictionary byte ceiling.
pub const DICTIONARY_BYTES: usize = 128 * 1024;
/// Framing-inclusive raw protocol error byte ceiling.
pub const ERROR_BYTES: usize = 128 * 1024;
/// Aggregate operation/attempt record ceiling.
pub const RECORD_BYTES: usize = 384 * 1024;
/// Binding, scenario receipt and framing ceiling.
pub const RECEIPT_BYTES: usize = 64 * 1024;
/// Data ceiling; the separate 64KiB diagnostic allowance is not extra data.
pub const WIRE_BYTES: usize = 704 * 1024;
/// Per-history monotonic duration ceiling.
pub const HISTORY_MICROS: u64 = 120_000_000;
/// Digest verified by the independently admitted capture/oracle layer.
pub type Digest = [u8; 32];
/// Index scoped to one history dictionary.
pub type AtomId = u16;

/// Domain of an exact opaque value.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AtomKind {
    /// Private bucket/namespace.
    Bucket,
    /// Complete object key.
    Key,
    /// Caller request identity.
    Request,
    /// Equality-only provider token, never parsed as a generation/hash.
    Token,
    /// Complete caller metadata.
    Metadata,
}
impl AtomKind {
    pub(crate) fn bound(self) -> usize {
        match self {
            Self::Bucket | Self::Request => 128,
            Self::Key => 1024,
            Self::Token => 256,
            Self::Metadata => 8192,
        }
    }
}

/// Interned value; Debug omits its contents.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct Atom {
    /// Domain.
    pub kind: AtomKind,
    /// Full bytes; truncation is forbidden.
    pub bytes: Vec<u8>,
}
impl fmt::Debug for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Atom")
            .field("kind", &self.kind)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Exact bucket-and-key association.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectId {
    /// Bucket reference.
    pub bucket: AtomId,
    /// Key reference.
    pub key: AtomId,
}
/// Whole payload descriptor, not an oracle for arbitrary subranges.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Payload {
    /// Full length.
    pub length: u64,
    /// Independently verified whole-body digest.
    pub digest: Digest,
}
/// Immutable version descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Version {
    /// Opaque token.
    pub token: AtomId,
    /// Exact body.
    pub payload: Payload,
    /// Exact metadata.
    pub metadata: AtomId,
}
/// Atomic condition. Unconditional overwrite is unrepresentable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Precondition {
    /// Key not previously created in this fresh history.
    Absent,
    /// Current opaque token equals this value.
    Matches(AtomId),
}
/// Full immutable mutation envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mutation {
    /// Caller identity.
    pub request: AtomId,
    /// Selected object.
    pub object: ObjectId,
    /// Original payload.
    pub payload: Payload,
    /// Original metadata.
    pub metadata: AtomId,
    /// Original condition.
    pub precondition: Precondition,
}
/// Original receipt, retained independently of the latest head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Receipt {
    /// Accepted caller identity.
    pub request: AtomId,
    /// Accepted version.
    pub version: Version,
}
/// Global recorder order and monotonic time, never a provider generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stamp {
    /// Global capture event order.
    pub order: u32,
    /// Microseconds since capture epoch.
    pub micros: u64,
}
/// Measured invocation/completion interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    /// Invocation or actual send start.
    pub start: Stamp,
    /// Completion, absent in incomplete captures.
    pub end: Option<Stamp>,
}
/// Public-contract operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Conditional mutation.
    Write(Mutation),
    /// Latest metadata observation.
    Head(ObjectId),
    /// Exact-version range, including deliberate invalid-range controls.
    ReadRange {
        /// Selected object.
        object: ObjectId,
        /// Selected version.
        token: AtomId,
        /// First byte.
        offset: u64,
        /// Requested length.
        length: u64,
    },
    /// Resolve the full original envelope, not an ID alone.
    Resolve(Mutation),
}
impl Action {
    pub(crate) fn object(self) -> ObjectId {
        match self {
            Self::Write(m) | Self::Resolve(m) => m.object,
            Self::Head(o) | Self::ReadRange { object: o, .. } => o,
        }
    }
    pub(crate) fn mutation(self) -> Option<Mutation> {
        match self {
            Self::Write(m) | Self::Resolve(m) => Some(m),
            _ => None,
        }
    }
}
/// Observed result. A timeout-as-rollback variant deliberately does not exist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Mutation result or exact resolution.
    Receipt(Receipt),
    /// Latest metadata.
    Head(Version),
    /// Complete range observation.
    Range {
        /// Returned exact version.
        token: AtomId,
        /// Actual complete count.
        length: u64,
        /// Independently verified range digest.
        digest: Digest,
    },
    /// Atomic condition did not match.
    Conflict,
    /// Exact selected object/version absent.
    NotFound,
    /// Invalid range refused.
    InvalidRange,
    /// Accepted identity reused with different content.
    IdentityConflict,
    /// Acceptance remains possible.
    UnknownOutcome,
    /// Retention/resolution unavailable; not proof of noncommit.
    ResolutionUnavailable,
    /// Required capability absent.
    Unsupported,
    /// Certified refusal before any transmission, not an HTTP error guess.
    BeforeSendRefusal,
}
/// Logical invocation; each caller retry/resolution has its own interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Operation {
    /// Contiguous ID from one.
    pub id: u16,
    /// Logical client zero through fifteen.
    pub client: u8,
    /// Workload label only.
    pub generation: u32,
    /// Predeclared race association.
    pub race: Option<u16>,
    /// Original invocation continued with the unchanged envelope.
    pub continuation_of: Option<u16>,
    /// Measured contract-call interval.
    pub span: Span,
    /// Complete request.
    pub action: Action,
    /// Missing when completion was not captured.
    pub outcome: Option<Outcome>,
}
/// Honest loss boundary of one captured attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Boundary {
    /// No bytes transmitted.
    NotTransmitted,
    /// Complete response received.
    ReplyReceived,
    /// Transmission completed, response not observed.
    AfterTransmitBeforeReply,
    /// Received response deliberately hidden at the caller boundary.
    ReplySuppressed,
    /// Transmission outcome uncertain.
    DuringTransmit,
}
/// Bounded protocol observation. Secret redaction is a capture-layer obligation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Attempt {
    /// Contiguous capture ID from one.
    pub id: u16,
    /// Associated logical operation.
    pub operation: u16,
    /// Actual send/reply interval, not barrier waiting time.
    pub span: Span,
    /// Measured loss boundary.
    pub boundary: Boundary,
    /// Final response status, if received.
    pub status: Option<u16>,
    /// Actual request bytes.
    pub request_bytes: u64,
    /// Actual response bytes.
    pub response_bytes: u64,
    /// Bounded raw error pool reference, after capture-layer secret redaction.
    pub error: Option<u16>,
}
/// Actual release event checked against an independent race plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Race {
    /// Predeclared group ID.
    pub id: u16,
    /// Actual release coordinate.
    pub released: Stamp,
}
/// Source/deployment/plan identities, externally authenticated before checking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Binding {
    /// Probe, transport, tool, image, config, topology and plan digests.
    pub digests: [Digest; 7],
    /// Unique capture identity.
    pub run: [u8; 16],
    /// One for Linux amd64; two for Linux arm64.
    pub platform: u8,
}
/// Original behavior area; unimplemented models never yield a pass here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scenario {
    /// Conditional creation.
    Create,
    /// Conditional replacement.
    Update,
    /// Exact-identity response-loss resolution.
    ResponseLoss,
    /// Immutable-version ranges/ABA.
    Range,
    /// Dedicated boot/CID/volume model remains required.
    Restart,
    /// Dedicated upload/failure/cleanup model remains required.
    Multipart,
}
/// One complete or explicitly incomplete capture; Debug omits opaque/error data.
#[derive(Clone, Eq, PartialEq)]
pub struct History {
    /// Exact admission binding.
    pub binding: Binding,
    /// Required behavior area.
    pub scenario: Scenario,
    /// Final capture marker observed.
    pub complete: bool,
    /// Predeclared logical count.
    pub declared_operations: u16,
    /// Actual attempt-record counter.
    pub declared_attempts: u16,
    /// Independently captured transmitted-attempt count.
    pub physical_attempts: u16,
    /// Unique opaque values.
    pub atoms: Vec<Atom>,
    /// Every invocation, including unknown and refused ones.
    pub operations: Vec<Operation>,
    /// Every attempt.
    pub attempts: Vec<Attempt>,
    /// Captured barrier releases.
    pub races: Vec<Race>,
    /// Raw bounded error bodies; capture layer must remove secrets.
    pub errors: Vec<Vec<u8>>,
    /// Bounded auxiliary receipt; not an implemented restart/multipart oracle.
    pub scenario_receipt: Vec<u8>,
}
impl fmt::Debug for History {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("History")
            .field("scenario", &self.scenario)
            .field("complete", &self.complete)
            .field("operations", &self.operations.len())
            .field("attempts", &self.attempts.len())
            .field("atoms", &self.atoms.len())
            .field("errors", &self.errors.len())
            .finish()
    }
}
/// Independent intended race membership, not a driver winner count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RacePlan {
    /// Required group ID.
    pub id: u16,
    /// Exact sixteen-bit client set.
    pub participants: u16,
}
/// Independent selected-byte expectation, not derived from a whole-object hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeOracle {
    /// Original complete payload.
    pub payload: Payload,
    /// First selected byte.
    pub offset: u64,
    /// Selected length.
    pub length: u64,
    /// Independently generated selected-byte digest.
    pub digest: Digest,
}
/// Independently planned invocation, excluding measured times and outcomes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Invocation {
    /// Exact planned identifier.
    pub id: u16,
    /// Planned logical client.
    pub client: u8,
    /// Workload label, never a version comparison.
    pub generation: u32,
    /// Planned barrier association.
    pub race: Option<u16>,
    /// Planned unchanged-envelope continuation.
    pub continuation_of: Option<u16>,
    /// Exact intended request using the admitted dictionary.
    pub action: Action,
}
/// Expectations supplied separately from the untrusted capture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plan {
    /// Independently authenticated expected binding.
    pub binding: Binding,
    /// Exact intended invocation count.
    pub operations: u16,
    /// Independently required behavior area.
    pub scenario: Scenario,
    /// Exact admitted dictionary, binding all invocation atom references.
    pub atoms: Vec<Atom>,
    /// Exact required invocations, not reconstructed from observed results.
    pub invocations: Vec<Invocation>,
    /// Required race groups/participants.
    pub races: Vec<RacePlan>,
    /// Independent range oracles.
    pub ranges: Vec<RangeOracle>,
}
/// Structural refusal, containing no record content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Invalid {
    /// Byte/count/value/duration bound exceeded.
    Bound,
    /// Admission identities differ.
    Binding,
    /// Shape/reference/tag/duplicate invalid.
    Shape,
    /// Recorder/client chronology invalid.
    Order,
    /// Continuation changed the original envelope or retried unknown blindly.
    Continuation,
}
impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Invalid {}

impl History {
    pub(crate) fn atom(&self, id: AtomId, kind: AtomKind) -> Result<(), Invalid> {
        if self
            .atoms
            .get(usize::from(id))
            .is_some_and(|a| a.kind == kind)
        {
            Ok(())
        } else {
            Err(Invalid::Shape)
        }
    }
    fn object(&self, o: ObjectId) -> Result<(), Invalid> {
        self.atom(o.bucket, AtomKind::Bucket)?;
        self.atom(o.key, AtomKind::Key)
    }
    fn payload(p: Payload) -> Result<(), Invalid> {
        if p.length <= 16 * 1024 * 1024 {
            Ok(())
        } else {
            Err(Invalid::Bound)
        }
    }
    fn version(&self, v: Version) -> Result<(), Invalid> {
        self.atom(v.token, AtomKind::Token)?;
        self.atom(v.metadata, AtomKind::Metadata)?;
        Self::payload(v.payload)
    }
    fn mutation(&self, m: Mutation) -> Result<(), Invalid> {
        self.object(m.object)?;
        self.atom(m.request, AtomKind::Request)?;
        self.atom(m.metadata, AtomKind::Metadata)?;
        if let Precondition::Matches(t) = m.precondition {
            self.atom(t, AtomKind::Token)?;
        }
        Self::payload(m.payload)
    }
    fn span(s: Span) -> Result<(), Invalid> {
        if s.start.micros > HISTORY_MICROS || s.start.order > 4096 {
            return Err(Invalid::Bound);
        }
        if let Some(e) = s.end {
            if e.order > 4096 || e.micros > HISTORY_MICROS {
                return Err(Invalid::Bound);
            }
            if e.order <= s.start.order || e.micros < s.start.micros {
                return Err(Invalid::Order);
            }
        }
        Ok(())
    }
    /// Bound and type-check input without asserting completeness or semantics.
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.atoms.len() > MAX_ATOMS
            || self.operations.len() > MAX_OPERATIONS
            || self.attempts.len() > MAX_ATTEMPTS
            || self.races.len() > 16
            || self.errors.len() > MAX_ATTEMPTS
            || self.scenario_receipt.len() > RECEIPT_BYTES - 512
            || usize::from(self.declared_operations) > MAX_OPERATIONS
            || usize::from(self.declared_attempts) > MAX_ATTEMPTS
            || self.physical_attempts > self.declared_attempts
        {
            return Err(Invalid::Bound);
        }
        if !matches!(self.binding.platform, 1 | 2) {
            return Err(Invalid::Binding);
        }
        let mut bytes = 0usize;
        let mut unique = BTreeSet::new();
        for a in &self.atoms {
            if a.bytes.len() > a.kind.bound()
                || (a.bytes.is_empty() && a.kind != AtomKind::Metadata)
                || !unique.insert(a)
            {
                return Err(Invalid::Shape);
            }
            bytes = bytes.checked_add(a.bytes.len() + 3).ok_or(Invalid::Bound)?;
            if bytes > DICTIONARY_BYTES {
                return Err(Invalid::Bound);
            }
        }
        bytes = 0;
        for e in &self.errors {
            if e.len() > 4096 {
                return Err(Invalid::Bound);
            }
            bytes = bytes.checked_add(e.len() + 2).ok_or(Invalid::Bound)?;
            if bytes > ERROR_BYTES {
                return Err(Invalid::Bound);
            }
        }
        let mut ids = BTreeSet::new();
        let mut stamps = Vec::with_capacity((MAX_OPERATIONS + MAX_ATTEMPTS) * 2 + 16);
        for o in &self.operations {
            if o.id == 0
                || usize::from(o.id) > MAX_OPERATIONS
                || !ids.insert(o.id)
                || o.client >= 16
                || o.span.end.is_some() != o.outcome.is_some()
            {
                return Err(Invalid::Shape);
            }
            Self::span(o.span)?;
            stamps.push(o.span.start);
            if let Some(e) = o.span.end {
                stamps.push(e);
            }
            self.object(o.action.object())?;
            match o.action {
                Action::Write(m) | Action::Resolve(m) => self.mutation(m)?,
                Action::ReadRange { token, .. } => self.atom(token, AtomKind::Token)?,
                Action::Head(_) => {}
            }
            match o.outcome {
                Some(Outcome::Receipt(r)) => {
                    self.atom(r.request, AtomKind::Request)?;
                    self.version(r.version)?;
                }
                Some(Outcome::Head(v)) => self.version(v)?,
                Some(Outcome::Range { token, length, .. }) => {
                    self.atom(token, AtomKind::Token)?;
                    if length > 16 * 1024 * 1024 {
                        return Err(Invalid::Bound);
                    }
                }
                _ => {}
            }
        }
        for o in &self.operations {
            for p in &self.operations {
                if p.client == o.client
                    && p.span.start.order < o.span.start.order
                    && p.span.end.is_none_or(|e| e.order >= o.span.start.order)
                {
                    return Err(Invalid::Order);
                }
            }
            // Omitting a continuation label cannot authorize a blind retry.
            if let Action::Write(m) = o.action {
                if self.operations.iter().any(|p| p.span.end.is_some_and(|e| e.order < o.span.start.order)
                    && p.outcome == Some(Outcome::UnknownOutcome)
                    && p.action.mutation().is_some_and(|prior| prior.request == m.request
                        && !self.operations.iter().any(|r| r.action == Action::Resolve(prior)
                            && matches!(r.outcome, Some(Outcome::Receipt(receipt)) if receipt.request == prior.request)
                            && p.span.end.is_some_and(|end| end.order < r.span.start.order)
                            && r.span.end.is_some_and(|end| end.order < o.span.start.order)))) {
                    return Err(Invalid::Continuation);
                }
            }
            if let Some(id) = o.continuation_of {
                let p = self
                    .operations
                    .iter()
                    .find(|p| p.id == id)
                    .ok_or(Invalid::Continuation)?;
                if p.span.end.is_none_or(|e| e.order >= o.span.start.order)
                    || o.action.mutation().is_none()
                    || o.action.mutation() != p.action.mutation()
                    || (p.outcome == Some(Outcome::UnknownOutcome)
                        && matches!(o.action, Action::Write(_)))
                {
                    return Err(Invalid::Continuation);
                }
            }
        }
        let mut ids = BTreeSet::new();
        let mut physical = 0u16;
        for a in &self.attempts {
            if a.id == 0
                || usize::from(a.id) > MAX_ATTEMPTS
                || !ids.insert(a.id)
                || a.error.is_some_and(|i| usize::from(i) >= self.errors.len())
            {
                return Err(Invalid::Shape);
            }
            Self::span(a.span)?;
            stamps.push(a.span.start);
            if let Some(e) = a.span.end {
                stamps.push(e);
            }
            let o = self
                .operations
                .iter()
                .find(|o| o.id == a.operation)
                .ok_or(Invalid::Shape)?;
            if a.span.start.order < o.span.start.order
                || a.span.start.micros < o.span.start.micros
                || o.span
                    .end
                    .zip(a.span.end)
                    .is_some_and(|(x, y)| y.order > x.order || y.micros > x.micros)
                || (o.span.end.is_some() && a.span.end.is_none())
            {
                return Err(Invalid::Order);
            }
            let reply = matches!(
                a.boundary,
                Boundary::ReplyReceived | Boundary::ReplySuppressed
            );
            if reply != a.status.is_some()
                || a.status.is_some_and(|s| !(200..=599).contains(&s))
                || (a.boundary == Boundary::NotTransmitted
                    && (a.request_bytes != 0 || a.response_bytes != 0))
            {
                return Err(Invalid::Shape);
            }
            if a.boundary != Boundary::NotTransmitted {
                physical += 1;
            }
        }
        if physical != self.physical_attempts {
            return Err(Invalid::Shape);
        }
        let mut races = BTreeSet::new();
        for r in &self.races {
            stamps.push(r.released);
            if r.id == 0
                || !races.insert(r.id)
                || r.released.micros > HISTORY_MICROS
                || r.released.order > 4096
            {
                return Err(Invalid::Shape);
            }
        }
        stamps.sort_by_key(|s| s.order);
        if stamps
            .windows(2)
            .any(|w| w[0].order == w[1].order || w[0].micros > w[1].micros)
        {
            return Err(Invalid::Order);
        }
        Ok(())
    }
}

/// Version-two operation fixed before capture. Parent references name actual
/// producing operations, never predicted provider tokens.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Specification {
    /// Contiguous predeclared ID.
    pub id: u16,
    /// Complete matrix scenario, zero through five.
    pub scenario: u8,
    /// Predeclared participant.
    pub client: u8,
    /// Fixed operation tag.
    pub action: String,
    /// Task-relative key.
    pub key: String,
    /// Earlier producing operation.
    pub parent: u16,
    /// Deterministic independent payload seed.
    pub seed: u64,
    /// Payload size.
    pub length: u64,
    /// Range start or multipart part number.
    pub offset: u64,
    /// Selected range size.
    pub range_length: u64,
    /// Concurrent group, zero if serial.
    pub race: u16,
    /// Fixed loss injection.
    pub fault: String,
}
/// Separately authenticated version-two workload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturePlan {
    /// Original seven-way source/deployment binding.
    pub binding: Binding,
    /// Private task bucket.
    pub bucket: String,
    /// Exact invocation program, independent of observed results.
    pub specifications: Vec<Specification>,
}
/// Bounded sanitized physical attempt and result.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CaptureEvent {
    /// Planned invocation.
    pub id: u16,
    /// Contiguous physical/refusal record identifier.
    pub attempt: u16,
    /// Actual send start in capture microseconds.
    pub start: u64,
    /// Observed completion in capture microseconds.
    pub end: u64,
    /// HTTP status, zero without a received response.
    pub status: u16,
    /// Explicit observed boundary.
    pub boundary: String,
    /// Actual transmitted HTTP bytes.
    pub sent: u64,
    /// Actual response body bytes.
    pub received: u64,
    /// Actual received HTTP bytes including headers/chunk framing, not TLS/kernel overhead.
    pub wire_received: u64,
    /// Complete equality-only ETag.
    pub etag: String,
    /// Distinct immutable version ID, never synthesized from ETag.
    pub version: String,
    /// Exact request metadata observed from provider.
    pub metadata: String,
    /// Caller mutation identity.
    pub request: String,
    /// Exact signed condition.
    pub condition: String,
    /// Exact selected immutable version.
    pub selected: String,
    /// Bounded typed restart/upload/list/completion data.
    pub auxiliary: String,
    /// Independently checked response body digest.
    pub digest: Digest,
    /// Provider-declared HEAD size, absent if unavailable.
    pub head_length: Option<u64>,
    /// Stable local refusal code, never provider text.
    pub error: String,
}
/// Complete or retained partial version-two capture.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Capture {
    /// Hash of separately frozen plan bytes.
    pub plan_digest: Digest,
    /// Every invocation, including refusal and unsupported.
    pub events: Vec<CaptureEvent>,
    /// Complete terminal marker.
    pub complete: bool,
    /// Admitted attempt/refusal count.
    pub attempts: u16,
}
