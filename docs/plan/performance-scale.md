# Performance, capacity and scale qualification

Status: proposed targets, not benchmark results. “Massive scale” must identify data volume, tenants, active shards, traffic, failure domains and operating cost. No public vendor latency is an hsearch SLO.

Cost qualification is mandatory under R23, with [initial decisions and experiments completed now](research/cost-decisions.md). Implementation gates sit directly in M02/M03/M06/M07/M09 and provider qualification in M15. [Cost research](research/cost-economics.md) gives provider/path evidence and [operating profiles](operating-profiles.md) separates latency, compute availability and durable failure domains. Reference SLOs below are not automatically inherited by cheaper profiles with weaker capacity or archive guarantees. Minimize measured TCO subject to the selected profile's frozen constraints; record a Pareto comparison, not only the fastest result.

## API work and scope budgets

The [scalability-first API profile](api-compatibility.md) narrows compatibility promises and requires finite per-profile request, response, decoded, CPU, queue, retry, remote-attempt/byte and snapshot-retention limits. Its example local ceilings are unmeasured inputs, not production SLOs. M02.08 establishes the cardinality/work model and M02.05/M02.07 freeze numerical envelopes before format/API implementation.

Measure total selected partitions S separately from concurrent RPCs F. Waves can reduce concurrency without reducing total work or cost. Fixed-scope hot requests must not scan or allocate per-index runtime state as the idle catalog grows. Test 10x cardinality, limit+one, hot-tenant skew, cancellation and pinned-generation retention; publish RSS/metadata-attempt and offered/completed/rejected curves with noise and failure/freshness profiles. Release acceptance requires the frozen scale-efficiency/latency/cost bounds, not just a plotted curve.

Exact counts, deep paging, bucket reduction and large jobs declare completeness and resource costs. Unsupported OpenSearch semantics use documented native alternatives; compare matching cases against pinned reference configuration and differences against independent native oracles. No count-descending ranking claim is inferred from key-ordered pages, and no denormalized view is claimed equivalent to arbitrary joins.

## Qualification ladder

| Tier | Proposed workload and topology | What passing establishes |
| --- | --- | --- |
| Local correctness | 1-3 nodes, RustFS, 1M documents, explicit disk/memory caps | Functional contract, not production latency or AZ durability |
| L1 release baseline | 3 failure domains, 6-12 role-separated Linux nodes, 100M docs / >=1 TiB logical source corpus, 1,000 active shards | Multi-node release workload and failure behavior at recorded hardware/cost |
| L2 expansion | 32-64 workers, >=10 TiB indexed data, 10,000 active shards; skewed tenants | Placement, metadata, fanout and resource scaling under measured load |
| L3 catalog scale | Model 1M tenants / up to 50M indexes, >=99% idle; 100k active logical shards in simulation | Catalog/activation complexity and cost model; not a deployed fleet claim |
| L4 fleet qualification | Increase real fleet/data only when budget and demand justify it; target 1,000 then 10,000 node architecture | Each actual size gets its own evidence; a simulation or linear extrapolation cannot certify it |

L1/L2 are implementation qualification targets. L3/L4 express the end-goal architecture and an ongoing test ladder, not a claim that initial release tests a 10,000-node cloud cluster. No O(total indexes) heartbeat, process, open reader, timer, subscription or consensus group is allowed. Catalog enumeration is paginated and background. Hot tenants can split independently; idle tenants activate on demand.

## SLO worksheet

Freeze workload/hardware definitions in M02 before accepting numbers. Proposed initial L1 objectives at <=70% sustained bottleneck utilization:

| Metric | Proposed objective | Scope |
| --- | --- | --- |
| Query service availability | 99.95% monthly | Valid supported requests; report dependency-caused unavailability separately, not exclude it silently |
| Warm term/boolean/filter top-10 search | p95 <=50 ms, p99 <=150 ms | Declared query mix, 1M-doc partitions, capped fanout <=16 shards, same-region clients |
| Cold query | p95 <=1 s, p99 <=3 s | Remote-read path, defined 1M-doc partition workload, caches intentionally cold |
| Object-durable ack | p99 <=1 s at benchmarked batch sizes | Regional object endpoint, bounded admission, published commit root included |
| Quorum durable ack | p99 <=25 ms | Three-zone local-region majority fsync, measured network/storage, no promised inter-region equivalence |
| Committed-to-searchable | p99 <=250 ms | Chosen refresh policy and load; searchable ack includes this wait |
| Single voter/node failure | No acknowledged loss; new writes recover <=30 s | One failed zone/node within 3-zone quorum failure envelope, actual fault exercised |
| Complete compute loss | RPO=0 for object mode; quorum RPO <=published archive lag | Data/key providers intact; recovery time measured separately |
| Archive lag in quorum mode | Normal p99 <=5 s; hard byte/time watermark blocks admission | No deletion of unarchived committed records |
| CCR lag | p99 <=5 s steady state at declared inter-region RTT/bandwidth | Async contract; overload/partition reported explicitly |
| Crypto overhead | Measure goal <=15% warm query CPU increase and <=20% bulk throughput loss | Paired encrypted/plaintext baselines, no safety waiver if missed |

These targets may be tightened or revised only with a recorded workload/evidence rationale. Failure requires engineering action or explicit requirement change; relabeling a slow cold test as warm is invalid. GET/conditional mutation latency, heavy aggregations, vector recall, huge-fanout queries and recovery throughput get separate profiles rather than borrowing these budgets.

## Capacity equations and cost

For interval `T`, busy producer count `P`, encoded write rate `W` and target batch bytes `B`, `max(W/B, P/T)` is only a rough busy-stream lower estimate; independent size/time triggers can add residual flushes. It is not a sparse-tenancy predictor. Fixed windows give `(1-exp(-lambda*T))/T` under Poisson rate `lambda`; a dirty timer starting at first arrival gives `lambda/(1+lambda*T)` before byte/IO effects. The [executed experiment](evidence/cost-research-results.md) uses dirty-start timers and byte triggers. Empty idle producers emit zero batches. The selected fenced append removes the per-batch root CAS and stores initial key wrapping inline; separately count structural/checkpoint/page/rewrap/retry requests and KMS. Never combine incompatible tenants/crypto policy to create a fictitious one-key/one-request saving.

Example model inputs, not measurements: 10 MiB/s ingestion, 4 MiB batches, 8 active producers, 250 ms flush bound yields a time floor of 32 batch PUT/s versus a 2.5/s size floor. At 2,592,000 s per 30-day month, 32/s is 82,944,000 PUTs before publication, key envelopes, compaction or retries. This demonstrates why batch policy and active-producer count matter. Use provider/region prices captured at the time of qualification; no current dollar estimate is assumed.

Model total cost as compute + memory/NVMe + object stored bytes + PUT/GET/HEAD/LIST/multipart + network/zone/region egress + KMS operations + telemetry + CI/AI. Count retries, read amplification, encryption tags/envelopes, compaction rewrite, tombstones, replication, snapshots and recovery. Object-durable versus quorum mode must include voter disks/replication traffic and archive overhead. Report cost per million docs ingested, million queries, indexed TiB-month, and failover/recovery event.

Meter provider HTTP attempts beneath retry wrappers and classify directed traffic by operation role, endpoints/payers, source/destination region/zone and encoded wire bytes. Preserve byte units per SKU; don't double a two-endpoint transfer rate as if it were a response. Differentiate same-region object reads from peer replication, NAT/LB/endpoints and cross-region traffic. Azure currently exempts same-region inter-zone transfer, so AWS rates cannot be reused across providers. A redacted trace/invoice drill-down carries high-cardinality tenant attribution; Prometheus summaries remain bounded.

Each profile's cost envelope includes idle control-plane activity, sparse/busy/skewed traffic, duplicate warm state per zone, failover prewarm/rebuild, catch-up, compaction/GC, KMS rotation and cross-cluster transfer. Budget controllers can throttle/defer optional work or reject new admission, never weaken quorum/security/freshness or discard retained data. The executable [component model](tools/cost_model.py) and [example inputs](cost-scenarios.json) support initial arithmetic; they do not replace real-provider billing reconciliation.

WAL space >= peak accepted byte rate × maximum archive/recovery lag × replica safety factor, plus indexes/metadata/reserve. Cache space cannot consume WAL reserve. RSS budget includes engine heap, native buffers, mmap residency/page cache, TLS/compression/crypto, task queues and diagnostics; configure admission below the container's kill limit. A tracked logical buffer sum is not a measured RSS cap.

Query remote latency is governed by dependent round trips as well as bytes: approximate critical path as metadata fetch + sequential remote dependency stages + CPU + queues. Measure ranges/bytes/requests per query and useful-to-fetched byte ratio. Coalesce adjacent blocks and prefetch metadata, but cap speculative fetch and cross-tenant cache pollution. Compaction uses separate I/O tokens so it cannot consume all query capacity.

## Optimization policy

Establish scalar/reference correctness and profile whole-service bottlenecks first. Candidate CPU work includes postings/bitset intersection, packed integer decode, norms/doc-values, vector distance, decompression, CRC/hash and AEAD. Candidate system work includes batch commit/refresh, async range coalescing, file packing, metadata hotcache, bounded parallel search, NUMA-aware pools and fair admission.

Use runtime dispatch for x86_64 SSE/AVX2 and optional AVX-512/VAES; ARM NEON and optional SVE/SVE2/crypto extensions require real capability checks. Test every supported path against scalar/reference results at boundary lengths, tails, alignment and float edge cases. Stable intrinsics are preferred; portable SIMD/nightly can be isolated if it has a measured advantage. Avoid CPU-specific release binaries that accidentally execute unsupported instructions.

`unsafe` is permitted only in narrowly reviewed kernel/I/O/FFI modules with a written safety contract and safe public boundary. Require an observed profile, scalar oracle, adversarial lengths/alignment tests, Miri/sanitizer applicability, architecture reviewer, and end-to-end benefit before adding it. Do not duplicate engine SIMD solely to own it in hsearch. Rust safety is not permission for unbounded allocation or unchecked arithmetic on on-disk lengths.

Measure io_uring versus normal async/pread, mmap versus buffered materialization, allocator choice, thread/core pinning, NUMA, huge pages, software/hardware crypto and H2 versus QUIC only after a bottleneck is identified. Preserve fallback. GPU/offload needs a separate workload and total transfer/queue cost justification. Newest hardware support is an ongoing capability matrix, not a launch dependency on every instruction set.

## Experimental protocol

Use pinned, license-compatible realistic corpora with hashes and generated adversarial distributions. Include append and update/delete-heavy workloads, many tiny tenants, one hot tenant, sparse/large documents, high-cardinality fields, complex analyzers, vectors/hybrid where supported, heavy aggs, skewed fanout, cold/warm/mixed caches and concurrent compaction/CCR/copy. Compare stock OpenSearch and the fork on matched durability, query semantics, hardware, data and load; no apples-to-oranges default refresh/durability.

Use open-loop arrivals to avoid coordinated omission; report offered/completed/rejected load, errors, latency histograms, confidence/repeats, recovery/compaction debt and exact sample windows. Separate warmup, steady state, overload and recovery. Repeat paired runs in interleaved order. Keep raw histograms/traces, configuration, commit and machine IDs. Shared CI variance cannot decide a 2% kernel regression; dedicated benchmark workers set noise-aware thresholds.

Tests must cross tipping points: cache smaller than working set, manifest larger than a page, shard count beyond active-engine cap, archive outage longer than reserve, follower slower than source, and allocator/CPU saturation. Release qualification includes 72-hour mixed-role soak, one-zone loss/rejoin, full restart, corruption/repair, key/cert rotation and old/new binary coexistence. No benchmark marked NOT-RUN can close a release criterion.
