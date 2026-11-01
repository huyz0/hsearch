# Versioned observability contract

Status: M08.01 implementation in progress; not accepted. The typed Rust event model, bounded local outbox and OpenTelemetry mapping seam require final source qualification, independent review and protected integration. There is no SDK, exporter, diagnostic service or runtime instrumentation. The native-report prerequisite is [accepted](evidence/m01-13-integrated.md); its private original-model fixture was report evidence, not schema acceptance.

## Producer model and privacy

hsearch-types owns schema version1; hsearch-observe depends only on types and std. This instrumentation contract is not a wire format, persisted receipt, authorization proof, billing record or runtime acknowledgement. Unknown major versions refuse with a fixed error. Changes require explicit schema/golden and field/byte/cardinality budget review; future fields cannot be silently omitted to retain a draft limit.

The producer accepts typed fields and discards all UntrustedDetails before queue admission. Raw document/source bytes, queries, credentials/keys, provider messages, tenant/index names and baggage from this untrusted path are never formatted, hashed, truncated or retained in the event. Opaque IDs are supplied by trusted protocol/tracing owners, not derived from bodies or credentials. Fixed byte types cannot prove provenance, uniqueness or authority, or detect a trusted caller deliberately encoding secrets. Trace, receipt and shard-incarnation IDs remain linkable high-cardinality diagnostics, requiring later authenticated scope/retention. They never become metric dimensions. A later SDK must not copy the complete diagnostic projection into metric labels.

## Schema and identity

The diagnostic projection has a fixed maximum32 attributes, stable order and static keys. Optional fields remain absent; measured zero is distinct from unknown. Checkpoints and numeric scope coordinates preserve every u64 bit as exactly20 decimal ASCII digits; signed metric values are separately bounded to i64::MAX and never silently saturated.

| Fields | Representation and meaning |
| --- | --- |
| hsearch.schema.version, hsearch.operation, hsearch.outcome, error.type | Version1 and fixed categorical vocabulary; success omits error.type. |
| trace_id, span_id | Nonzero owner-supplied16/8-byte correlation, projected as32/16 hexadecimal characters. |
| hsearch.shard.incarnation, hsearch.activation, hsearch.shard | Non-nil16-byte incarnation, full-u64 activation/shard; diagnostic identity only. |
| hsearch.progress.committed/applied/searchable/archived | Four distinct validated checkpoint coordinates, never a durability acknowledgement from telemetry. |
| hsearch.partitions.selected/completed, hsearch.completeness, hsearch.count.relation/value | Admitted logical work and complete/partial/unknown result, exact/lower-bound/unknown count precision. |
| hsearch.receipt.state/id/committed | Owner-observed unknown outcome, committed, searchable or visibility timeout; a timeout retains committed identity. |
| hsearch.rpc.peak, hsearch.remote.attempts/retries | Peak simultaneous calls versus total physical attempts, including repeated attempts. |
| hsearch.payload.bytes, hsearch.cross_zone.bytes, hsearch.queue.ns, hsearch.cpu.ns | Measured encoded/directed bytes and nanoseconds, without body contents or invented provider tariffs. |
| hsearch.pins.count/bytes, hsearch.metadata.attempts, hsearch.object.attempts | Per-observation retained-pin samples and disjoint physical attempt subsets, not fleet gauges. |

The protocol owner keeps an incarnation stable across retries, process restarts and ownership changes that continue one logical shard history, and assigns a fresh globally unique UUID when a new history is created, including recreation with reused numbers. ShardIncarnation only rejects nil and preserves all128bits; it does not generate/check uniqueness, UUID variant/version, routing or access. Scope equality includes incarnation, activation and shard; mismatched receipt/progress composition refuses in either order even if numeric coordinates match. This diagnostic scope omits tenant/index/writer epoch and must never substitute for the full architecture routing/mutation identity or an authenticated receipt. Names are not identities.

The finite operation categories are admission, ingest, search, count, receipt, snapshot, partition_rpc, object_io, checkpoint, apply, refresh and maintenance. Outcomes are success, partial, rejected, failed, cancelled and unknown. Eleven fixed error classes cover invalid_input, unsupported, permission_denied, overloaded, conflict, unknown_outcome, unavailable, corrupt, cancelled, deadline_exceeded and internal, with hsearch. prefixes. Unknown outcome is distinct from a safe retry; the owning protocol resolves the exact mutation.

Completion validation enforces completed<=selected and complete implies all selected work completed. Exact counts require completeness. Successful complete results and unsuccessful incomplete results remain distinct; failed/partial/missing-target work cannot claim an exact whole-scope total. Receipt observations only attach to ingest/receipt operations and retain owner state. Known Pending/Visible commitment may accompany Success or a downstream Failed outcome with a bounded error other than UnknownOutcome. Failure preserves identity and commitment; it never grants rollback or blind retry. Unknown receipts still require Unknown/UnknownOutcome and no known committed coordinate; explicit Timeout still requires Cancelled/DeadlineExceeded. Other pairings remain refused under this minimal schema. Known receipt commitment cannot exceed supplied progress; visibility timeout requires the deadline outcome and never rolls back commitment. These are local consistency checks, not external truth certification.

## Bounded outbox and loss

An explicitly created active outbox has positive capacity<=1024. record minimizes first, then uses one std sync_channel try_send; full/disconnected returns fixed Full/Disconnected and discards the record without changing a protocol result. No network/exporter callback executes on the producer path. Standard-channel lock contention remains possible; this is not a wait-free or universal latency guarantee. Consumer work occurs after dequeue. Producers/borrowed progress adapters are created for active work, with no producer/timer/subscription per idle shard.

Observation payload has a compile-time<=512-byte ceiling. RECORD_BYTES is the actual target size and MAX_QUEUED_PAYLOAD_BYTES=1024*RECORD_BYTES. This bounds buffered record payload only: channel bookkeeping, handles, Projection, serialized diagnostics, allocator overhead and process RSS are separate. Maximum projection uses all32 slots, verified by exact golden/unique-key tests; future additions require budget review. Dropped observations cannot support exact billing, exact business totals or guaranteed incident completeness. Callers may inspect delivery status; the legacy progress observer intentionally discards it.

## OpenTelemetry seam

No SDK dependency or OTLP compatibility is claimed. Fourteen static instruments expose exact signed-integer samples to a later adapter:

| Instrument | Kind | Unit |
| --- | --- | --- |
| hsearch.observations | Counter, one consumed local observation | {observation} |
| hsearch.partitions.selected, hsearch.partitions.completed | Histogram | {partition} |
| hsearch.rpc.peak | Histogram | {call} |
| hsearch.remote.attempts, hsearch.remote.retries | Histogram | {attempt} |
| hsearch.payload.bytes, hsearch.cross_zone.bytes | Histogram | By |
| hsearch.queue.duration, hsearch.cpu.duration | Histogram | ns |
| hsearch.pins.count | Histogram | {pin} |
| hsearch.pins.bytes | Histogram | By |
| hsearch.metadata.attempts, hsearch.object.attempts | Histogram | {attempt} |

Measurements are per-observation distributions, not cumulative protocol counters or per-node gauges. MetricDimensions contains only operation, outcome and fixed error-or-absent:12x6x12<=864 combinations per instrument, including combinations constructor consistency may refuse. No request/trace/tenant/index/incarnation/activation/shard/checkpoint/receipt ID or arbitrary resource dimension enters these labels. The mapping has at most14 samples without allocation. A later SDK still needs total resource/deployment/cardinality, histogram bucket, exemplar, queue/export and retention budgets.

Official source context remains the [metrics API](https://opentelemetry.io/docs/specs/otel/metrics/api/), [SDK cardinality policy](https://opentelemetry.io/docs/specs/otel/metrics/sdk/), [predictable error attributes](https://opentelemetry.io/docs/specs/semconv/registry/attributes/error/) and [sensitive-data guidance](https://opentelemetry.io/docs/security/handling-sensitive-data/). This seam does not certify a selected SDK or protocol version. Actual transport/retry metering belongs below the runtime wrappers; provider counts, directed path costs and authenticated diagnostics remain later tasks.

## Required qualification

Final implementation must pass independent constructor/version/outcome/completion/resource/scope/receipt oracles, non-nil and full-bit identity roundtrips, both composition orders, exact32-field and14-instrument/unit goldens, measured target record/queue payload, unknown-versus-zero, original hostile-detail minimization, fixed label cardinality, full/disconnected/FIFO/stalled-consumer and bounded concurrent admission. Scoped progress must preserve owner identity and all checkpoint coordinates without accepting arbitrary data.

Observe's old exact-source N/A audit must be removed for executable code, with actual production-object maps and >=90 aggregate/crate/file/changed coverage in both profiles. Full native x86_64/ARM64 checks, separate final-source reviews and protected integration remain required. SDK/OTLP interoperability, service-level overhead/resilience, end-to-end spans, provider/hardware/fleet, release and deployment are not established by these library checks. M01.09 original ARM cause/M01.08 stay open.
