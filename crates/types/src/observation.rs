//! Fixed observation vocabulary and lossless signed-64 metric values.
//!
//! These are instrumentation types, not an API error wire format, receipt or authority proof.

/// Version of the proposed observation field contract; no transport is implemented here.
pub const SCHEMA_VERSION: u16 = 1;

/// A fixed metric operation dimension, without tenant, index or request labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    /// The fixed admission category.
    Admission,
    /// The fixed ingest category.
    Ingest,
    /// The fixed search category.
    Search,
    /// The fixed count category.
    Count,
    /// The fixed receipt category.
    Receipt,
    /// The fixed snapshot category.
    Snapshot,
    /// The fixed partition_rpc category.
    PartitionRpc,
    /// The fixed object_io category.
    ObjectIo,
    /// The fixed checkpoint category.
    Checkpoint,
    /// The fixed apply category.
    Apply,
    /// The fixed refresh category.
    Refresh,
    /// The fixed maintenance category.
    Maintenance,
}

impl Operation {
    /// The complete version-one finite dimension.
    pub const ALL: [Self; 12] = [
        Self::Admission,
        Self::Ingest,
        Self::Search,
        Self::Count,
        Self::Receipt,
        Self::Snapshot,
        Self::PartitionRpc,
        Self::ObjectIo,
        Self::Checkpoint,
        Self::Apply,
        Self::Refresh,
        Self::Maintenance,
    ];

    /// Return the stable category without allocation or caller-provided text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Ingest => "ingest",
            Self::Search => "search",
            Self::Count => "count",
            Self::Receipt => "receipt",
            Self::Snapshot => "snapshot",
            Self::PartitionRpc => "partition_rpc",
            Self::ObjectIo => "object_io",
            Self::Checkpoint => "checkpoint",
            Self::Apply => "apply",
            Self::Refresh => "refresh",
            Self::Maintenance => "maintenance",
        }
    }
}

/// An observation outcome; delivery never acknowledges a protocol operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The fixed success category.
    Success,
    /// The fixed partial category.
    Partial,
    /// The fixed rejected category.
    Rejected,
    /// The fixed failed category.
    Failed,
    /// The fixed cancelled category.
    Cancelled,
    /// The fixed unknown category.
    Unknown,
}

impl Outcome {
    /// The complete version-one finite dimension.
    pub const ALL: [Self; 6] = [
        Self::Success,
        Self::Partial,
        Self::Rejected,
        Self::Failed,
        Self::Cancelled,
        Self::Unknown,
    ];

    /// Return the stable category without allocation or caller-provided text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

/// A bounded diagnostic class, without provider messages or retry authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureClass {
    /// The fixed hsearch.invalid_input category.
    InvalidInput,
    /// The fixed hsearch.unsupported category.
    Unsupported,
    /// The fixed hsearch.permission_denied category.
    PermissionDenied,
    /// The fixed hsearch.overloaded category.
    Overloaded,
    /// The fixed hsearch.conflict category.
    Conflict,
    /// The fixed hsearch.unknown_outcome category.
    UnknownOutcome,
    /// The fixed hsearch.unavailable category.
    Unavailable,
    /// The fixed hsearch.corrupt category.
    Corrupt,
    /// The fixed hsearch.cancelled category.
    Cancelled,
    /// The fixed hsearch.deadline_exceeded category.
    DeadlineExceeded,
    /// The fixed hsearch.internal category.
    Internal,
}

impl FailureClass {
    /// The complete version-one finite dimension.
    pub const ALL: [Self; 11] = [
        Self::InvalidInput,
        Self::Unsupported,
        Self::PermissionDenied,
        Self::Overloaded,
        Self::Conflict,
        Self::UnknownOutcome,
        Self::Unavailable,
        Self::Corrupt,
        Self::Cancelled,
        Self::DeadlineExceeded,
        Self::Internal,
    ];

    /// Return the stable category without allocation or caller-provided text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "hsearch.invalid_input",
            Self::Unsupported => "hsearch.unsupported",
            Self::PermissionDenied => "hsearch.permission_denied",
            Self::Overloaded => "hsearch.overloaded",
            Self::Conflict => "hsearch.conflict",
            Self::UnknownOutcome => "hsearch.unknown_outcome",
            Self::Unavailable => "hsearch.unavailable",
            Self::Corrupt => "hsearch.corrupt",
            Self::Cancelled => "hsearch.cancelled",
            Self::DeadlineExceeded => "hsearch.deadline_exceeded",
            Self::Internal => "hsearch.internal",
        }
    }
}

/// A metric value cannot be represented without signed-integer loss.
/// No rejected input is retained in the error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CounterOutOfRange;

/// A nonnegative value that converts exactly to an OpenTelemetry signed integer.
/// Core checkpoint coordinates retain their separate full-u64 semantics.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct MetricCounter(u64);

impl MetricCounter {
    /// Zero work, not an unknown or missing measurement.
    pub const ZERO: Self = Self(0);

    /// Refuse an unrepresentable value without echoing it.
    pub const fn try_new(value: u64) -> Result<Self, CounterOutOfRange> {
        if value <= i64::MAX as u64 {
            Ok(Self(value))
        } else {
            Err(CounterOutOfRange)
        }
    }

    /// Return the exact nonnegative value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Return the lossless signed representation.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.0 as i64
    }

    /// Add without wrapping or silently saturating a measured count.
    pub const fn checked_add(self, other: Self) -> Result<Self, CounterOutOfRange> {
        // Both operands are <= i64::MAX, so their sum fits in u64.
        Self::try_new(self.0 + other.0)
    }
}

/// Fixed validation failures, without rejected values or external error text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidObservation {
    /// A major schema version is not implemented.
    Version,
    /// The outcome and bounded failure class disagree.
    Outcome,
    /// Completion, counts or selected/completed work disagree.
    Completion,
    /// Resource counters are inconsistent.
    Resources,
    /// Two observations refer to different incarnation/activation/shard scopes.
    Scope,
    /// Receipt state, operation and outcome disagree.
    Receipt,
    /// A trace or span identifier is all zero.
    Correlation,
}

/// Owner-supplied non-nil shard-incarnation UUID bytes.
/// This carries identity only: it does not generate UUIDs, validate routing or grant authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShardIncarnation([u8; 16]);

impl ShardIncarnation {
    /// Refuse the nil UUID; the protocol owner supplies globally unique incarnation identity.
    pub fn try_new(bytes: [u8; 16]) -> Result<Self, InvalidObservation> {
        if bytes == [0; 16] {
            return Err(InvalidObservation::Scope);
        }
        Ok(Self(bytes))
    }

    /// Preserve all 128 identity bits without hashing, truncation or numeric coercion.
    #[must_use]
    pub const fn bytes(self) -> [u8; 16] {
        self.0
    }
}

/// Trusted diagnostic coordinates, never authorization or metric dimensions.
/// This is not the complete tenant/index routing key or an authenticated receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scope {
    /// Incarnation changes when a shard identity is recreated, even if numbers are reused.
    pub incarnation: ShardIncarnation,
    /// Opaque activation number supplied by its protocol owner.
    pub activation: u64,
    /// Opaque logical shard number supplied by its protocol owner.
    pub shard: u64,
}

/// Correlation supplied by a trusted tracing boundary, never derived from bodies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Correlation {
    trace_id: [u8; 16],
    span_id: [u8; 8],
}

impl Correlation {
    /// Reject invalid all-zero identifiers without retaining arbitrary text.
    pub fn try_new(trace_id: [u8; 16], span_id: [u8; 8]) -> Result<Self, InvalidObservation> {
        if trace_id == [0; 16] || span_id == [0; 8] {
            return Err(InvalidObservation::Correlation);
        }
        Ok(Self { trace_id, span_id })
    }

    /// Return the diagnostic trace identifier.
    #[must_use]
    pub const fn trace_id(self) -> [u8; 16] {
        self.trace_id
    }

    /// Return the diagnostic span identifier.
    #[must_use]
    pub const fn span_id(self) -> [u8; 8] {
        self.span_id
    }
}

/// Whether the admitted result scope is complete, independent of count precision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completeness {
    /// The entire admitted scope contributed a successful result.
    Complete,
    /// Some admitted scope is missing or failed.
    Partial,
    /// Completion cannot be established.
    Unknown,
}

impl Completeness {
    /// Stable diagnostic value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
        }
    }
}

/// Count precision for the admitted result scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CountRelation {
    /// Exact count, permitted only for a complete successful result.
    Exact,
    /// A measured lower bound, without an exact-total claim.
    LowerBound,
    /// No count can be stated.
    Unknown,
}

impl CountRelation {
    /// Stable diagnostic value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "eq",
            Self::LowerBound => "gte",
            Self::Unknown => "unknown",
        }
    }
}

/// Validated result scope and count precision; zero does not mean unknown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Completion {
    selected: MetricCounter,
    completed: MetricCounter,
    completeness: Completeness,
    relation: CountRelation,
    count: Option<MetricCounter>,
}

impl Completion {
    /// Validate scope completion and presence/precision of a result count.
    pub fn try_new(
        selected: MetricCounter,
        completed: MetricCounter,
        completeness: Completeness,
        relation: CountRelation,
        count: Option<MetricCounter>,
    ) -> Result<Self, InvalidObservation> {
        if completed > selected
            || (completeness == Completeness::Complete && completed != selected)
            || (relation == CountRelation::Exact && completeness != Completeness::Complete)
            || ((relation == CountRelation::Unknown) != count.is_none())
        {
            return Err(InvalidObservation::Completion);
        }
        Ok(Self {
            selected,
            completed,
            completeness,
            relation,
            count,
        })
    }

    /// Logical partitions selected at admission, not simultaneous network calls.
    #[must_use]
    pub const fn selected(self) -> MetricCounter {
        self.selected
    }

    /// Logical partitions whose work completed, including failed results.
    #[must_use]
    pub const fn completed(self) -> MetricCounter {
        self.completed
    }

    /// Result completeness over the admitted scope.
    #[must_use]
    pub const fn completeness(self) -> Completeness {
        self.completeness
    }

    /// Precision of the result count.
    #[must_use]
    pub const fn relation(self) -> CountRelation {
        self.relation
    }

    /// Count when one can be stated.
    #[must_use]
    pub const fn count(self) -> Option<MetricCounter> {
        self.count
    }
}

/// Optional finite work measurements. Some(zero) records observed zero; None is unknown.
/// Remote attempts include retries; metadata and object attempts are disjoint subsets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceUsage {
    /// Peak simultaneous remote calls, not total fanout.
    pub peak_rpcs: Option<MetricCounter>,
    /// All physical remote attempts measured below retries.
    pub remote_attempts: Option<MetricCounter>,
    /// Repeated physical attempts, included in remote_attempts.
    pub retries: Option<MetricCounter>,
    /// Encoded payload bytes, without any payload contents.
    pub payload_bytes: Option<MetricCounter>,
    /// Bytes directed across zones, without deriving a cloud tariff.
    pub cross_zone_bytes: Option<MetricCounter>,
    /// Time spent queued, in nanoseconds.
    pub queue_ns: Option<MetricCounter>,
    /// Measured CPU time, in nanoseconds.
    pub cpu_ns: Option<MetricCounter>,
    /// Retained pin count.
    pub pin_count: Option<MetricCounter>,
    /// Bytes retained by pins, independent of logical catalog size.
    pub pin_bytes: Option<MetricCounter>,
    /// Physical metadata attempts, included in remote_attempts.
    pub metadata_attempts: Option<MetricCounter>,
    /// Physical object attempts, included in remote_attempts.
    pub object_attempts: Option<MetricCounter>,
}

impl ResourceUsage {
    fn validate(self) -> Result<Self, InvalidObservation> {
        // Two signed64 nonnegative operands have a sum that fits in u64.
        // Missing subsets contribute only a lower bound, never an emitted zero.
        let subset = self.metadata_attempts.map(MetricCounter::get).unwrap_or(0)
            + self.object_attempts.map(MetricCounter::get).unwrap_or(0);
        if self
            .peak_rpcs
            .zip(self.remote_attempts)
            .is_some_and(|(peak, total)| peak > total)
            || self
                .retries
                .zip(self.remote_attempts)
                .is_some_and(|(retry, total)| retry > total)
            || self
                .remote_attempts
                .is_some_and(|total| subset > total.get())
        {
            return Err(InvalidObservation::Resources);
        }
        Ok(self)
    }
}

/// Visibility of an already committed write; a timeout never rolls it back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Visibility {
    /// Committed and not yet observed searchable.
    Pending,
    /// Searchability was observed by the owning protocol.
    Visible,
    /// A visibility wait expired while the committed identity remains valid.
    Timeout,
}

/// A bounded owner-supplied receipt observation, never an acknowledgement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceiptObservation {
    id: [u8; 16],
    scope: Scope,
    committed: Option<crate::Sequence>,
    visibility: Option<Visibility>,
}

impl ReceiptObservation {
    /// Retain identity for resolution of an unknown mutation outcome.
    #[must_use]
    pub const fn unknown(id: [u8; 16], scope: Scope) -> Self {
        Self {
            id,
            scope,
            committed: None,
            visibility: None,
        }
    }

    /// Retain committed identity independently of search visibility.
    #[must_use]
    pub const fn committed(
        id: [u8; 16],
        scope: Scope,
        sequence: crate::Sequence,
        visibility: Visibility,
    ) -> Self {
        Self {
            id,
            scope,
            committed: Some(sequence),
            visibility: Some(visibility),
        }
    }

    /// Opaque owner-generated identity; never a metric label or body hash.
    #[must_use]
    pub const fn id(self) -> [u8; 16] {
        self.id
    }

    /// Incarnation/activation/shard scope supplied by the owner.
    #[must_use]
    pub const fn scope(self) -> Scope {
        self.scope
    }

    /// Known committed coordinate, preserved through visibility timeout.
    #[must_use]
    pub const fn sequence(self) -> Option<crate::Sequence> {
        self.committed
    }

    /// Visibility when commitment is known.
    #[must_use]
    pub const fn visibility(self) -> Option<Visibility> {
        self.visibility
    }
}

/// Version-one allowlisted record. No dynamic text, payload or attribute map is stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Observation {
    operation: Operation,
    outcome: Outcome,
    failure: Option<FailureClass>,
    correlation: Option<Correlation>,
    scope: Option<Scope>,
    progress: Option<crate::Progress>,
    completion: Option<Completion>,
    receipt: Option<ReceiptObservation>,
    resources: Option<ResourceUsage>,
}

impl Observation {
    /// Validate a version and outcome before accepting optional typed fields.
    pub fn try_new(
        version: u16,
        operation: Operation,
        outcome: Outcome,
        failure: Option<FailureClass>,
    ) -> Result<Self, InvalidObservation> {
        if version != SCHEMA_VERSION {
            return Err(InvalidObservation::Version);
        }
        if (outcome == Outcome::Success) != failure.is_none()
            || (outcome == Outcome::Unknown && failure != Some(FailureClass::UnknownOutcome))
            || (failure == Some(FailureClass::UnknownOutcome) && outcome != Outcome::Unknown)
        {
            return Err(InvalidObservation::Outcome);
        }
        Ok(Self {
            operation,
            outcome,
            failure,
            correlation: None,
            scope: None,
            progress: None,
            completion: None,
            receipt: None,
            resources: None,
        })
    }

    /// Add trusted tracing coordinates; no arbitrary baggage is accepted.
    #[must_use]
    pub const fn with_correlation(mut self, correlation: Correlation) -> Self {
        self.correlation = Some(correlation);
        self
    }

    fn with_scope(mut self, scope: Scope) -> Result<Self, InvalidObservation> {
        if self.scope.is_some_and(|current| current != scope) {
            return Err(InvalidObservation::Scope);
        }
        self.scope = Some(scope);
        Ok(self)
    }

    /// Add validated checkpoints under one explicit incarnation/activation/shard scope.
    pub fn with_progress(
        self,
        scope: Scope,
        progress: crate::Progress,
    ) -> Result<Self, InvalidObservation> {
        let mut next = self.with_scope(scope)?;
        if next.receipt.is_some_and(|receipt| {
            receipt
                .sequence()
                .is_some_and(|sequence| sequence > progress.committed())
        }) {
            return Err(InvalidObservation::Receipt);
        }
        next.progress = Some(progress);
        Ok(next)
    }

    /// Add completion while refusing exact/complete claims on unsuccessful outcomes.
    pub fn with_completion(mut self, completion: Completion) -> Result<Self, InvalidObservation> {
        if (self.outcome == Outcome::Success) != (completion.completeness == Completeness::Complete)
        {
            return Err(InvalidObservation::Completion);
        }
        self.completion = Some(completion);
        Ok(self)
    }

    /// Add an owner-supplied receipt without changing its protocol outcome.
    pub fn with_receipt(self, receipt: ReceiptObservation) -> Result<Self, InvalidObservation> {
        if !matches!(self.operation, Operation::Ingest | Operation::Receipt)
            || match receipt.visibility {
                None => self.outcome != Outcome::Unknown,
                Some(Visibility::Timeout) => {
                    self.outcome != Outcome::Cancelled
                        || self.failure != Some(FailureClass::DeadlineExceeded)
                }
                Some(Visibility::Pending | Visibility::Visible) => {
                    !matches!(self.outcome, Outcome::Success | Outcome::Failed)
                }
            }
            || self.progress.is_some_and(|progress| {
                receipt
                    .committed
                    .is_some_and(|sequence| sequence > progress.committed())
            })
        {
            return Err(InvalidObservation::Receipt);
        }
        let mut next = self.with_scope(receipt.scope)?;
        next.receipt = Some(receipt);
        Ok(next)
    }

    /// Add measured resource counters after consistency validation.
    pub fn with_resources(mut self, resources: ResourceUsage) -> Result<Self, InvalidObservation> {
        self.resources = Some(resources.validate()?);
        Ok(self)
    }

    /// Fixed operation dimension.
    #[must_use]
    pub const fn operation(self) -> Operation {
        self.operation
    }
    /// Fixed outcome dimension.
    #[must_use]
    pub const fn outcome(self) -> Outcome {
        self.outcome
    }
    /// Fixed failure dimension; successful events omit error.type.
    #[must_use]
    pub const fn failure(self) -> Option<FailureClass> {
        self.failure
    }
    /// Optional diagnostic correlation, never metric labels.
    #[must_use]
    pub const fn correlation(self) -> Option<Correlation> {
        self.correlation
    }
    /// Optional activation/shard scope, never metric labels.
    #[must_use]
    pub const fn scope(self) -> Option<Scope> {
        self.scope
    }
    /// Optional distinct committed/applied/searchable/archived coordinates.
    #[must_use]
    pub const fn progress(self) -> Option<crate::Progress> {
        self.progress
    }
    /// Optional result scope and count precision.
    #[must_use]
    pub const fn completion(self) -> Option<Completion> {
        self.completion
    }
    /// Optional owner-supplied receipt observation.
    #[must_use]
    pub const fn receipt(self) -> Option<ReceiptObservation> {
        self.receipt
    }
    /// Optional measured work, where absence remains distinguishable from zero.
    #[must_use]
    pub const fn resources(self) -> Option<ResourceUsage> {
        self.resources
    }
}
