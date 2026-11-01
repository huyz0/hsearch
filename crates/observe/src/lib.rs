//! Bounded producer minimization and typed OpenTelemetry mapping seam.
//! No SDK, exporter, network callback or diagnostic service is implemented here.

use hsearch_types::{Progress, observation::*};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};

/// Maximum buffered observations per explicitly created active outbox.
pub const MAX_CAPACITY: usize = 1024;
/// Maximum number of allowlisted diagnostic fields in version one.
pub const MAX_FIELDS: usize = 32;
/// Exact record payload bytes on the current target, excluding channel bookkeeping.
pub const RECORD_BYTES: usize = std::mem::size_of::<Observation>();
/// Upper bound on queued record payload, not allocator overhead or total process RSS.
pub const MAX_QUEUED_PAYLOAD_BYTES: usize = MAX_CAPACITY * RECORD_BYTES;
const _: () = assert!(RECORD_BYTES <= 512);

/// Invalid capacity; no caller-provided value is retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidCapacity;

/// Fixed delivery result; never changes a storage or search result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivery {
    /// Accepted into the local outbox; not an exporter acknowledgement.
    Queued,
    /// Capacity is exhausted; the record was discarded.
    Full,
    /// Consumer is gone; the record was discarded.
    Disconnected,
}

/// Borrowed untrusted details deliberately excluded before enqueueing.
/// No formatting, hashing, truncation or retention of these values is performed.
/// This boundary cannot prevent trusted callers misusing an opaque diagnostic ID.
#[derive(Default)]
pub struct UntrustedDetails<'a> {
    /// Raw document bytes, never exported.
    pub document: &'a [u8],
    /// Raw query text, never exported.
    pub query: &'a str,
    /// Credential/key bytes, never exported.
    pub credentials: &'a [u8],
    /// Raw provider error, never exported.
    pub provider_message: &'a str,
    /// Tenant name, never exported.
    pub tenant: &'a str,
    /// Index name, never exported.
    pub index: &'a str,
    /// Untrusted tracing baggage, never exported.
    pub baggage: &'a str,
}

/// Clone only for active producers; no per-idle-shard producer is allocated.
#[derive(Clone)]
pub struct Producer {
    sender: SyncSender<Observation>,
}

/// Single consumer for a finite outbox; SDK/exporter work happens after dequeue.
pub struct Consumer {
    receiver: Receiver<Observation>,
}

/// Create an explicitly bounded outbox. Capacity zero and values above1024 refuse.
/// std channel bookkeeping and producer handles are outside the payload-byte bound.
pub fn outbox(capacity: usize) -> Result<(Producer, Consumer), InvalidCapacity> {
    if capacity == 0 || capacity > MAX_CAPACITY {
        return Err(InvalidCapacity);
    }
    let (sender, receiver) = mpsc::sync_channel(capacity);
    Ok((Producer { sender }, Consumer { receiver }))
}

impl Producer {
    /// Minimize at the producer boundary, then try once without waiting for capacity.
    /// The standard channel may briefly contend on its internal lock; no wait-free or
    /// universal latency bound is claimed. No consumer/exporter callback runs here.
    pub fn record(&self, event: Observation, _: UntrustedDetails<'_>) -> Delivery {
        match self.sender.try_send(event) {
            Ok(()) => Delivery::Queued,
            Err(TrySendError::Full(_)) => Delivery::Full,
            Err(TrySendError::Disconnected(_)) => Delivery::Disconnected,
        }
    }

    /// Borrow one producer for an active operation's progress notifications.
    #[must_use]
    pub const fn scoped(&self, scope: Scope) -> ScopedProgressObserver<'_> {
        ScopedProgressObserver {
            producer: self,
            scope,
        }
    }
}

impl Consumer {
    /// Take one record without waiting. No callback, decoding or remote I/O is run.
    pub fn try_recv(&self) -> Result<Observation, TryRecvError> {
        self.receiver.try_recv()
    }
}

/// Observe validated progress without receiving document contents or credentials.
/// Implementations must bound work/buffering; delivery never establishes durability.
pub trait ProgressObserver: Send + Sync {
    /// Record a snapshot under the implementation's documented loss policy.
    fn record(&self, progress: Progress);
}

/// Borrowed scoped adapter; absent/full/disconnected telemetry cannot alter progress.
pub struct ScopedProgressObserver<'a> {
    producer: &'a Producer,
    scope: Scope,
}

impl ScopedProgressObserver<'_> {
    /// Report delivery explicitly when the caller needs a bounded loss signal.
    pub fn try_record(&self, progress: Progress) -> Delivery {
        // Fixed schema/outcome and a new empty event cannot fail these constructors.
        let event = Observation::try_new(
            SCHEMA_VERSION,
            Operation::Checkpoint,
            Outcome::Success,
            None,
        )
        .expect("fixed observation constructor")
        .with_progress(self.scope, progress)
        .expect("new scoped progress");
        self.producer.record(event, UntrustedDetails::default())
    }
}

impl ProgressObserver for ScopedProgressObserver<'_> {
    fn record(&self, progress: Progress) {
        let _ = self.try_record(progress);
    }
}

/// Finite metric dimensions, without IDs, sequence values, tenant or index labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetricDimensions {
    /// Static operation name.
    pub operation: &'static str,
    /// Static outcome name.
    pub outcome: &'static str,
    /// Static error class; absent on success.
    pub error_type: Option<&'static str>,
}

/// Project only the three fixed metric dimensions; at most864 combinations.
#[must_use]
pub fn metric_dimensions(event: Observation) -> MetricDimensions {
    MetricDimensions {
        operation: event.operation().as_str(),
        outcome: event.outcome().as_str(),
        error_type: event.failure().map(FailureClass::as_str),
    }
}

/// Mapping seam values. A later SDK adapter maps Text and Integer directly,
/// and Hex/Decimal ASCII arrays to strings without any lossy integer conversion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Value {
    /// Static categorical text only.
    Text(&'static str),
    /// Lossless signed integer for a bounded counter.
    Integer(i64),
    /// Lowercase fixed-width hexadecimal diagnostic identity (span uses first16 bytes).
    Hex {
        /// ASCII bytes, with zero padding outside length.
        bytes: [u8; 32],
        /// Valid length, always16 or32.
        len: u8,
    },
    /// Full-u64 diagnostic coordinate as exactly20 decimal ASCII digits.
    Decimal([u8; 20]),
}

/// One static-key diagnostic attribute; caller-provided keys/text are impossible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Attribute {
    /// Versioned static field key.
    pub key: &'static str,
    /// Minimized typed value.
    pub value: Value,
}

/// Finite projection without heap allocation or consumer callbacks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Projection {
    fields: [Option<Attribute>; MAX_FIELDS],
    len: usize,
}

impl Projection {
    fn push(&mut self, key: &'static str, value: Value) {
        // The complete version-one mapping below has at most32 fields.
        assert!(self.len < MAX_FIELDS, "fixed projection field bound");
        self.fields[self.len] = Some(Attribute { key, value });
        self.len += 1;
    }

    /// Iterate only initialized fields; order is stable for version-one golden tests.
    pub fn attributes(&self) -> impl Iterator<Item = Attribute> + '_ {
        self.fields[..self.len].iter().flatten().copied()
    }
}

fn hexadecimal(bytes: &[u8]) -> Value {
    let mut output = [0; 32];
    for (index, byte) in bytes.iter().enumerate() {
        output[index * 2] = b"0123456789abcdef"[(byte >> 4) as usize];
        output[index * 2 + 1] = b"0123456789abcdef"[(byte & 15) as usize];
    }
    Value::Hex {
        bytes: output,
        len: (bytes.len() * 2) as u8,
    }
}

fn coordinate(mut number: u64) -> Value {
    let mut digits = [b'0'; 20];
    for digit in digits.iter_mut().rev() {
        *digit += (number % 10) as u8;
        number /= 10;
    }
    Value::Decimal(digits)
}

/// Project all optional diagnostic fields with stable keys and lossless values.
/// Diagnostic identifiers require scoped retention/authentication in future runtime.
#[must_use]
pub fn project(event: Observation) -> Projection {
    let mut output = Projection {
        fields: [None; MAX_FIELDS],
        len: 0,
    };
    output.push(
        "hsearch.schema.version",
        Value::Integer(i64::from(SCHEMA_VERSION)),
    );
    output.push("hsearch.operation", Value::Text(event.operation().as_str()));
    output.push("hsearch.outcome", Value::Text(event.outcome().as_str()));
    if let Some(failure) = event.failure() {
        output.push("error.type", Value::Text(failure.as_str()));
    }
    if let Some(correlation) = event.correlation() {
        output.push("trace_id", hexadecimal(&correlation.trace_id()));
        output.push("span_id", hexadecimal(&correlation.span_id()));
    }
    if let Some(scope) = event.scope() {
        output.push(
            "hsearch.shard.incarnation",
            hexadecimal(&scope.incarnation.bytes()),
        );
        output.push("hsearch.activation", coordinate(scope.activation));
        output.push("hsearch.shard", coordinate(scope.shard));
    }
    if let Some(progress) = event.progress() {
        for (key, value) in [
            ("hsearch.progress.committed", progress.committed()),
            ("hsearch.progress.applied", progress.applied()),
            ("hsearch.progress.searchable", progress.searchable()),
            ("hsearch.progress.archived", progress.archived()),
        ] {
            output.push(key, coordinate(value.get()));
        }
    }
    if let Some(completion) = event.completion() {
        output.push(
            "hsearch.partitions.selected",
            Value::Integer(completion.selected().as_i64()),
        );
        output.push(
            "hsearch.partitions.completed",
            Value::Integer(completion.completed().as_i64()),
        );
        output.push(
            "hsearch.completeness",
            Value::Text(completion.completeness().as_str()),
        );
        output.push(
            "hsearch.count.relation",
            Value::Text(completion.relation().as_str()),
        );
        if let Some(count) = completion.count() {
            output.push("hsearch.count.value", Value::Integer(count.as_i64()));
        }
    }
    if let Some(receipt) = event.receipt() {
        let state = match receipt.visibility() {
            None => "unknown_outcome",
            Some(Visibility::Pending) => "committed",
            Some(Visibility::Visible) => "searchable",
            Some(Visibility::Timeout) => "visibility_timeout",
        };
        output.push("hsearch.receipt.state", Value::Text(state));
        output.push("hsearch.receipt.id", hexadecimal(&receipt.id()));
        if let Some(sequence) = receipt.sequence() {
            output.push("hsearch.receipt.committed", coordinate(sequence.get()));
        }
    }
    if let Some(resources) = event.resources() {
        for (key, value) in [
            ("hsearch.rpc.peak", resources.peak_rpcs),
            ("hsearch.remote.attempts", resources.remote_attempts),
            ("hsearch.remote.retries", resources.retries),
            ("hsearch.payload.bytes", resources.payload_bytes),
            ("hsearch.cross_zone.bytes", resources.cross_zone_bytes),
            ("hsearch.queue.ns", resources.queue_ns),
            ("hsearch.cpu.ns", resources.cpu_ns),
            ("hsearch.pins.count", resources.pin_count),
            ("hsearch.pins.bytes", resources.pin_bytes),
            ("hsearch.metadata.attempts", resources.metadata_attempts),
            ("hsearch.object.attempts", resources.object_attempts),
        ] {
            if let Some(number) = value {
                output.push(key, Value::Integer(number.as_i64()));
            }
        }
    }
    output
}

/// Instrument semantics for the later SDK adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstrumentKind {
    /// Add one for each consumed local observation, not each protocol operation.
    Counter,
    /// One nonnegative per-observation sample, not a cumulative business total.
    Histogram,
}

/// Stable instrument identity. Resource dimensions and SDK buckets need later budgets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Instrument {
    /// Static instrument name.
    pub name: &'static str,
    /// Static OpenTelemetry unit spelling.
    pub unit: &'static str,
    /// Recording semantics.
    pub kind: InstrumentKind,
}

/// Version-one finite instrument catalog, with no caller-provided metric names.
pub const INSTRUMENTS: [Instrument; 14] = [
    Instrument {
        name: "hsearch.observations",
        unit: "{observation}",
        kind: InstrumentKind::Counter,
    },
    Instrument {
        name: "hsearch.partitions.selected",
        unit: "{partition}",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.partitions.completed",
        unit: "{partition}",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.rpc.peak",
        unit: "{call}",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.remote.attempts",
        unit: "{attempt}",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.remote.retries",
        unit: "{attempt}",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.payload.bytes",
        unit: "By",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.cross_zone.bytes",
        unit: "By",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.queue.duration",
        unit: "ns",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.cpu.duration",
        unit: "ns",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.pins.count",
        unit: "{pin}",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.pins.bytes",
        unit: "By",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.metadata.attempts",
        unit: "{attempt}",
        kind: InstrumentKind::Histogram,
    },
    Instrument {
        name: "hsearch.object.attempts",
        unit: "{attempt}",
        kind: InstrumentKind::Histogram,
    },
];

/// One exact finite metric input; backend delivery/cardinality policy is not implied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Measurement {
    /// Catalog instrument, without high-cardinality diagnostic coordinates.
    pub instrument: &'static Instrument,
    /// Nonnegative exact signed64 measurement.
    pub value: i64,
    /// Only the three fixed metric dimensions.
    pub dimensions: MetricDimensions,
}

/// Project at most14 allocation-free metric inputs for the later SDK adapter.
/// Absence stays absent; dropped observations cannot support exact billing totals.
/// Pin/resource/work values are per-observation distributions, never per-node gauges.
pub fn measurements(event: Observation) -> impl Iterator<Item = Measurement> {
    let dimensions = metric_dimensions(event);
    let value = |index: usize, number: MetricCounter| Measurement {
        instrument: &INSTRUMENTS[index],
        value: number.as_i64(),
        dimensions,
    };
    let completion = event.completion();
    let resources = event.resources();
    [
        Some(value(
            0,
            MetricCounter::try_new(1).expect("one fits signed64"),
        )),
        completion.map(|c| value(1, c.selected())),
        completion.map(|c| value(2, c.completed())),
        resources
            .and_then(|r| r.peak_rpcs)
            .map(|number| value(3, number)),
        resources
            .and_then(|r| r.remote_attempts)
            .map(|number| value(4, number)),
        resources
            .and_then(|r| r.retries)
            .map(|number| value(5, number)),
        resources
            .and_then(|r| r.payload_bytes)
            .map(|number| value(6, number)),
        resources
            .and_then(|r| r.cross_zone_bytes)
            .map(|number| value(7, number)),
        resources
            .and_then(|r| r.queue_ns)
            .map(|number| value(8, number)),
        resources
            .and_then(|r| r.cpu_ns)
            .map(|number| value(9, number)),
        resources
            .and_then(|r| r.pin_count)
            .map(|number| value(10, number)),
        resources
            .and_then(|r| r.pin_bytes)
            .map(|number| value(11, number)),
        resources
            .and_then(|r| r.metadata_attempts)
            .map(|number| value(12, number)),
        resources
            .and_then(|r| r.object_attempts)
            .map(|number| value(13, number)),
    ]
    .into_iter()
    .flatten()
}
