# Product requirements (authoritative)

Confirmed by Huy on 13 November 2026. These are the product-level requirements AI agents deliver against. Where an older plan document disagrees, this file wins and the conflict is listed in [Conflicts with existing plan documents](#conflicts-with-existing-plan-documents). The older [requirement coverage matrix](requirements.md) (R01-R23) stays as the delivery-evidence index; each `PR-` ID below says which R-IDs it extends.

Targets here are acceptance targets, not measurements. Nothing in this file is claimed as achieved. A number is only reported once a test or benchmark has run, on the hardware named in the report. Per the existing rule, numbers from an emulator or a laptop are relative, not absolute.

## 1. Scale and tenancy

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-01 | A deployment holds **10M indexes**. Size is highly skewed: most hold a few documents, a few hold billions, and fewer than 10% are "large". | Catalog and cost model at 10M indexes with 90% or more idle (extends L3 in [performance-scale](performance-scale.md)). No per-index heartbeat, timer, open reader, process or consensus group. |
| PR-02 | Shard count adapts to index size. A small index uses one shard and shares commit domains with its tenant. A hot or large index splits online. | Split under write load loses no acknowledged write (M13). A 1-document index costs no steady-state requests. |
| PR-03 | Per-tenant quotas, rate limits and noisy-neighbor isolation. | One tenant at 10x quota does not move another tenant's p99 beyond a bound set in the test. |
| PR-04 | Cold indexes cost nothing while idle (no requests, no compute) and wake within a stated latency. | Idle-cost test: zero request count over an hour. Cold-wake p99 target is set when M02 freezes workloads. |

## 2. Storage and durability

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-05 | The remote object store is the only durable tier. Local NVMe (or EBS as a fallback) and RAM are caches. Losing every cache loses no acknowledged data. | Kill all nodes and wipe all local disks: every acknowledged write is recovered from the store. |
| PR-06 | An acknowledged write is never lost. It is in a durable translog/WAL before the acknowledgement. | Fault-injection history: lost replies, node kill, store errors; zero acknowledged writes missing (existing D16 fenced-append design). |
| PR-07 | **Durable versus searchable is a per-request choice**, like OpenSearch `refresh`: `false` (ack when durable), `wait_for` (ack when searchable), `true` (force visibility for this request). Durable receipts always identify the committed write, even if a visibility wait times out. | Each mode tested for ack semantics, timeout behavior and cost. `true` is allowed but is coalesced and metered (see conflicts). |
| PR-08 | **Search visibility after a write: p50 < 500 ms, p99 < 5 s** under the declared benchmark load. | Measured write-ack to first-search-hit latency, reported with load, hardware and batch settings. |
| PR-09 | Storage backends: S3, GCS, Azure Blob, and S3-compatible stores (RustFS first, MinIO). Conditional writes are the only coordination primitive. | Provider capability probe and conformance per provider (existing R04). |
| PR-10 | Snapshot and restore, and index format versioning with a rolling-upgrade path. | Old-format corpus opens in new binaries; mixed-version nodes fail explicitly, never silently. |

## 3. Cost

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-11 | Minimize object-store **PUT, LIST and GET** requests. No request count may scale with documents, indexes, or idle time. Never LIST on a normal read or write path. | Request-per-operation budgets in CI, as in M02.07/M03.07/M03.08. |
| PR-12 | Minimize **inter-AZ data transfer**. Peers prefer same-zone choices (routing, replica selection, cache fetch), as a preference and never a hard rule. Object-store traffic within a region is not charged by zone, so cold reads go to the store, not to a peer in another zone. | Metered cross-zone bytes per query and per write in the benchmark report. Reference design: the `feature/serverlessshell` branch of huyz0/OpenSearch (`serverless/README.md`, "Keeping traffic inside a zone"): the zone comes from the stock `node.attr.zone` awareness attribute, travels in the node's lease, and an unknown zone counts as neither near nor far. |
| PR-13 | Adding index nodes must not raise request cost beyond a stated bound. Concurrent writes to one shard share PUTs. | Request count per acknowledged write stays flat as writer nodes grow, in the benchmark. |
| PR-14 | Cost targets ($/GB-month stored, $/million writes, $/million queries) are measured in benchmarks using the [cost model](research/cost-economics.md), not asserted. | Benchmark report includes the cost breakdown by request type and cross-AZ bytes. |
| PR-15 | Per-index cost attribution (requests, bytes, cross-AZ bytes). | Usage records emitted per index; reconcile with the store bill in M15. |

## 4. Throughput and node roles

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-16 | Many concurrent large `_bulk` requests, streamed rather than buffered whole, with bounded memory per request. | Concurrent-bulk test with a memory ceiling; no OOM, backpressure visible to clients. |
| PR-17 | Many concurrent requests per node, with bounded queues and admission control. | Concurrency sweep with latency curve and explicit 429s past the limit. |
| PR-18 | **Node roles are declared explicitly in node configuration**: `index`, `search` and `coordinator`, in any combination (a single process may hold all three for development). A node refuses work outside its declared roles with a structured error. Roles scale independently. | Config test: each role alone, each pair, all three. A search-only node never accepts a write. The node reports its roles in its status. |
| PR-19 | **A control plane that spawns or places nodes is out of scope.** The engine must expose what such a control plane needs: declared roles and zone, health and readiness, **scaling metrics per role**, and graceful drain. External orchestrators (Kubernetes and similar) do the spawning and decide when to scale. Index-role metrics include write and bulk ingest rate, queue depth, backpressure and 429 rate, commit and publish lag, active shards and engine memory. Search-role metrics include query rate, p50/p99 latency, queue depth, cache hit rate and fill (RAM and NVMe), cold-read rate and active readers. Coordinator-role metrics include request rate, in-flight requests, fanout width, admission rejections and cross-zone bytes. Every role also reports CPU and memory against its limit. | Status and metrics API contract test, with a named metric set per role in a scrape endpoint (Prometheus format); a test that raising load on one role moves that role's metrics and not another's; drain test with no acknowledged-write loss. |

## 5. API and engine

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-20 | **OpenSearch API compatibility as close as practical** for full-text and vector search: query DSL, mappings, settings, bulk, search, get. Client libraries do not need to work unchanged. Anything that conflicts with scalability is dropped or replaced with a documented native alternative (for example the `_cat` APIs, replica-count settings, cluster-manager APIs). | Compatibility matrix in [api-compatibility](api-compatibility.md): each route is matching, different or unsupported, with a reason and test. |
| PR-21 | Full-text search through lucene-rust as the core engine (existing D01). | Differential tests against Java Lucene fixtures. |
| PR-22 | **Vector search: HNSW and IVF, each with quantization and with a disk-based variant.** Build on what lucene-rust already provides. | See [Vector support in lucene-rust](#vector-support-in-lucene-rust). Recall@10 floors per variant at a stated corpus. |
| PR-23 | Optional extensions in Rust or as binaries, through typed trusted Rust modules and a versioned external RPC (existing D05). No stable native-plugin ABI is promised. | External example module in CI (existing R07). |

## 6. Security and encryption

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-24 | **BYOK with a different key per index, but not for every index.** An index with no key of its own uses its tenant or deployment default. Key hierarchy must not cost a KMS call per request. | Mixed-key test; KMS-call count bounded by cache policy; revoked key blocks reads of that index only. |
| PR-25 | Re-encrypting an index under a new key is done by **same-index replication** to a new key domain, with no downtime for reads. | Re-key test: copy under key B, switch, key A revoked, data still readable. |
| PR-26 | Crypto-shredding: destroying an index's key makes its data unrecoverable. | Test that data is unreadable after key destruction. |
| PR-27 | mTLS between nodes, authentication and RBAC for clients, audit log of key rotation and revocation. | Negative identity/role/tenant tests (existing R11). |

## 7. Replication

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-28 | Cross-cluster replication (active-passive, follower pull) and same-index replication (PR-25). | Bootstrap, stream, gap, promotion histories (existing R08, D07). The replica's lag is reported, not hidden. |
| PR-29 | Recovery targets: RPO 0 inside one region in object mode; node takeover time target set in M02 workload freeze. | Fault-injection takeover timing, reported with the fleet-test method. The fork measured p50 59 s and p99 121 s at a 30 s lease, so takeover time is a real design target, not a given. |

## 8. Hardware and performance

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-30 | SIMD-optimized on **ARM and x86**, for AWS r8g (Graviton4) and r8i, and GCP C4. Kernels are selected at runtime by CPU feature detection and always have a scalar fallback. | Paired scalar and accelerated correctness tests; benchmarks run on real instances of each type (not emulation). ARM CI is a manual workflow, not per PR. |
| PR-31 | Search latency targets on a warm and a cold cache, set per query class in M02 when workloads freeze. Initial proposals stay in [performance-scale](performance-scale.md). | Reported with hardware, corpus and cache state. |

## 9. Correctness and operations

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-32 | Fencing and linearizability tests, differential tests against Java Lucene, and fault injection. | Existing M03/M17 histories. |
| PR-33 | Metrics, tracing and logs, with per-index cost attribution (PR-15). | Existing R20. |
| PR-34 | Index lifecycle (rollover, delete) and aliases. | API-level tests for rollover and alias swap. |

## Vector support in lucene-rust

Checked against `huyz0/lucene-rust` on 13 November 2026 (source inspection only; nothing was built or run for this section):

| Need | Already in lucene-rust | Gap to confirm in M02 |
| --- | --- | --- |
| HNSW | `lucene-codecs` HNSW graph, `lucene-index` vector writing, `lucene-search` KNN search and recall tests | Concurrent build and merge behavior at hsearch's segment sizes |
| HNSW + quantization | `hnsw_quant_vectors.rs` in `lucene-index`, quantizers in `lucene-vector-quant` (BBQ/OSQ, Lucene99 scalar) | Which quantizer settings OpenSearch mappings should expose |
| HNSW disk-based | `lucene-disk-hnsw`: one aligned page read per hop (DiskANN/LM-DiskANN style), optional inline codes | Range reads from the object store, not a local file |
| IVF | `lucene-ivf`: clustered posting lists, `nprobe`, rerank, calibrated depth, merge | Update and delete behavior; IVF is write-once per segment |
| IVF + quantization | Pluggable `Quantizer` trait shared with HNSW | Same as above |
| IVF disk-based | Posting lists are segment files and can be range-read | **Not confirmed**: no disk-specific IVF layout found. Verify first in M02. |
| Sparse and multi-vector | `lucene-sparse`, `lucene-tensor` | Out of scope for PR-22; usable later |
| SIMD | `lucene-util/src/simd.rs`: runtime AVX-512 detection on x86 and a NEON path on aarch64 | Whether AVX2 and Graviton4 SVE paths exist, and per-instance benchmarks |
| Encryption | `lucene-store-crypto`: AES-256-GCM per file with a `KeyProvider` for BYOK | Per-index key mapping onto the hsearch key hierarchy |

Build on these crates through the existing engine adapter. Add to lucene-rust only where a row above is a confirmed gap.

## Conflicts with existing plan documents

1. **Forced refresh.** [api-compatibility](api-compatibility.md) A05 and the architecture text excluded per-request `refresh=true`. PR-07 overrides this: all three modes are supported. Forced visibility is coalesced across requests and metered, so it cannot create one tiny segment or one extra object per write. The architecture text on "per-request forced refresh" must be updated when the API task for A05 lands.
2. **Visibility target.** The SLO worksheet in [performance-scale](performance-scale.md) proposes committed-to-searchable p99 <= 250 ms. PR-08 is p50 < 500 ms and p99 < 5 s, which is looser at the tail and tighter at the median. PR-08 is the requirement. The worksheet's number may stay as an internal stretch target.
3. **Scale figure.** The plan models 1M tenants and up to 50M indexes. PR-01 is 10M indexes. The plan's figure stays a modelled upper bound.
4. **Object-durable ack p99 <= 1 s** (worksheet) and PR-08 interact: with refresh `wait_for`, ack time includes the visibility wait. The batching window must be chosen against both. This is an open tradeoff for M02.07/M03.08, not a settled number.
5. **Control plane.** The plan has maintenance, placement and copy jobs. PR-19 keeps those inside the engine but excludes any component that spawns nodes.
6. **Roles.** [architecture](architecture.md) lists API coordinator, index worker, query worker, maintenance worker and journal. PR-18 uses the user's three names. Mapping: coordinator = API/query coordinator, index = index and maintenance workers, search = query worker. The journal role stays an optional fourth role for quorum mode.
7. **Replication of queries.** The fork serves search from the shard owner for gets, while this plan separates query replicas. No conflict, but PR-12 applies to both paths.

## Open questions for later milestones

These are not blocking. Each is settled when its milestone freezes workloads.

- Warm and cold p99 per query class (PR-31); cold-wake latency (PR-04); takeover time (PR-29).
- Whether `refresh=true` must be strictly immediate or may wait up to the batching window (PR-07).
- Which OpenSearch `knn_vector` mapping parameters map to which lucene-rust variant (PR-20, PR-22).
