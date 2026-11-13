# Product requirements (authoritative)

Confirmed by Huy on 13 November 2026. These are the product-level requirements AI agents deliver against. Where an older plan document disagrees, this file wins and the conflict is listed in [Conflicts with existing plan documents](#conflicts-with-existing-plan-documents). The older [requirement coverage matrix](requirements.md) (R01-R23) stays as the delivery-evidence index; each `PR-` ID below says which R-IDs it extends.

Targets here are acceptance targets, not measurements. Nothing in this file is claimed as achieved. A number is only reported once a test or benchmark has run, on the hardware named in the report. Per the existing rule, numbers from an emulator or a laptop are relative, not absolute.

## 1. Scale and tenancy

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-01 | A deployment holds **10M indexes**. Size is highly skewed: most hold a few documents, a few hold billions, and fewer than 10% are "large". | Catalog and cost model at 10M indexes with 90% or more idle (extends L3 in [performance-scale](performance-scale.md)). No per-index heartbeat, timer, open reader, process or consensus group. |
| PR-02 | Shard count adapts to index size. A small index uses one shard and shares commit domains with its tenant. A hot or large index splits online. | Split under write load loses no acknowledged write (M13). A 1-document index costs no steady-state requests. |
| PR-03 | Per-tenant quotas, rate limits and noisy-neighbor isolation. | One tenant at 10x its quota raises another tenant's p99 by at most 20% over its unloaded baseline (proposed bound, confirmed in M02). |
| PR-04 | Cold indexes cost nothing while idle (no requests, no compute) and wake within a stated latency. | Idle-cost test: zero request count over an hour. Cold-wake p99 target is set when M02 freezes workloads. |

## 2. Storage and durability

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-05 | In object mode the remote object store is the only durable tier. Local NVMe (or EBS as a fallback) and RAM are caches. Losing every cache loses no acknowledged data. The optional quorum mode (low-latency writes) additionally needs its journal volumes for the unarchived tail, so its guarantee is RPO <= archive lag; that is a declared exception, not the default. | Object mode: kill all nodes and wipe all local disks; every acknowledged write is recovered from the store. |
| PR-06 | An acknowledged write is never lost. It is in a durable translog/WAL before the acknowledgement. | Fault-injection history: lost replies, node kill, store errors; zero acknowledged writes missing (existing D16 fenced-append design). |
| PR-07 | **Durable versus searchable is a per-request choice**, like OpenSearch `refresh`: `false` (ack when durable), `wait_for` (ack when searchable), `true` (ack once a reader covering this write is published, forcing a reader publication if none is due, but coalesced with other requests so it never costs one segment or object per write). Durable receipts always identify the committed write, even if a visibility wait times out. | Each mode tested for ack semantics, timeout behavior and cost. `true` waits at most one batching window (bounded by the profile) and is metered (see conflicts). |
| PR-08 | **Search visibility after a write: p50 < 500 ms, p99 < 5 s** under the declared benchmark load. | Measured write-ack to first-search-hit latency, reported with load, hardware and batch settings. |
| PR-09 | Storage backends: S3, GCS, Azure Blob, and S3-compatible stores (RustFS first, MinIO). Conditional writes are the only coordination primitive. | Provider capability probe and conformance per provider (existing R04). |
| PR-10 | Snapshot and restore, and index format versioning with a rolling-upgrade path. | Old-format corpus opens in new binaries; mixed-version nodes fail explicitly, never silently. |

## 3. Cost

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-11 | Minimize object-store **PUT, LIST and GET** requests. No request count may scale with documents, indexes, or idle time. Never LIST on a normal read or write path. | Request-per-operation budgets in CI, as in M02.07/M03.07/M03.08. |
| PR-12 | Minimize **inter-AZ data transfer**. Peers prefer same-zone choices (routing, replica selection, cache fetch), as a preference and never a hard rule. Object-store traffic within a region is not charged by zone, so cold reads go to the store, not to a peer in another zone. | Metered cross-zone bytes per query and per write in the benchmark report. Reference design (read on 13 Nov, branch head 5cd4bb3, not run): the `feature/serverlessshell` branch of huyz0/OpenSearch (`serverless/README.md`, "Keeping traffic inside a zone"): the zone comes from the stock `node.attr.zone` awareness attribute, travels in the node's lease, and an unknown zone counts as neither near nor far. |
| PR-13 | Adding index nodes must not raise request cost per acknowledged write. A shard has one owner at a time, which batches concurrent writes to that shard into shared PUTs; more index nodes spread more shards, not more requests per write. | Requests per acknowledged write at 3x index nodes are within 10% of the 1x figure for the same workload (proposed bound, confirmed in M02). |
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
| PR-29 | **Any node can die at any time and recovery is fast.** Recovery latency may be higher than normal while it happens, but the interval is short and bounded. After an abrupt node loss (kill -9, instance loss), every shard it owned is writable again on another node within **p50 <= 5 s and p99 <= 15 s**, and searches over its data are served by other nodes from published state with no unavailability beyond retries within the request deadline. RPO 0 in object mode. | Fleet fault test: kill a loaded node, measure kill-to-first-acknowledged-write per shard, and verify zero acknowledged-write loss. These are proposed targets to be confirmed or revised when M02 freezes workloads. The OpenSearch fork's own fleet test (read on 13 Nov, not re-run; branch `feature/serverlessshell` at 5cd4bb3, `serverless/scale-test-results.md`) reports kill-to-writable p50 59 s and p99 121 s at a 30 s lease, most of it lease expiry and activation queueing, so meeting this needs a shorter failure-detection design and parallel shard activation. It is a real design target, not a given. |
| PR-35 | **Graceful shutdown hands workload off explicitly.** On drain, a node stops accepting new work for its roles, hands each owned shard to another node with a deliberate fenced transfer (no waiting for a lease to expire), finishes or hands off in-flight requests, and exits when empty or at a deadline. | Drain test under load: per-shard write pause p99 <= 2 s (proposed), zero acknowledged-write loss, zero requests failed without a retriable error, and no node-wide pause. Drain of a node holding N shards completes in a bounded time that is reported against N. A drain that hits its deadline falls back to the abrupt-loss path (PR-29), never to lost data. |
| PR-36 | Recovery does not depend on the dead node's local disk: a replacement node, with cold caches, recovers any shard using only the object store. Cache warm-up after recovery is bounded and does not block serving. | Wipe a node's disks and kill it: its shards recover elsewhere with correct data. Report cold-start query latency during warm-up. |
| PR-37 | **An AZ failure is survivable.** The deployment spans 3 AZs. The external control plane tags each node with its AZ (the stock `node.attr.zone` attribute, as in the fork); the engine reads the tag and never calls a cloud API for it. Losing one AZ must not lose acknowledged data and must not take out a disproportionate share of any tenant's capacity. | AZ-loss fault test with nodes in 3 AZs: kill every node in one AZ at once. Zero acknowledged-write loss; every shard writable again within the PR-29 targets; searches continue from the other two AZs; capacity loss for any index is within its share of the lost AZ (about one third), not more. Object-mode data is already in the regional store, so the test is about serving capacity and ownership, not data copies. |
| PR-38 | **Placement is AZ-aware.** Shard ownership, search replicas and cache copies of one index are spread across AZs so no single AZ holds all of an index's serving capacity. A hot index has serving copies in at least 2 AZs, and an index meeting the profile's availability class has them in all 3. Placement uses node tags only as hints (never to grant authority), and a node with no tag counts as neither near nor far. | Placement test over a 3-AZ fleet at skewed index sizes: no index has all serving copies in one AZ unless its profile says single-AZ economy; spread stays within 1 node of even across AZs for large indexes. |
| PR-39 | **Avoid cross-AZ cost where possible, but never at the price of availability.** Requests are routed to a same-AZ coordinator and same-AZ serving copy when one is eligible; cold data is fetched from the regional object store, which has no cross-AZ charge; peer traffic crosses AZs only when no eligible same-AZ node exists, or for writes whose owner is in another AZ. Cross-AZ replication of cache or data to gain AZ resilience is metered and bounded, and is opt-in per profile because it costs money. | Benchmark reports cross-AZ bytes per query and per write at 3 AZs, split by cause. With all three AZs healthy, steady-state cross-AZ bytes for queries are near zero beyond result-sized responses to an out-of-AZ client; the target figure is set in M02. |

## 8. Hardware and performance

| ID | Requirement | Testable form |
| --- | --- | --- |
| PR-30 | SIMD-optimized on **ARM and x86**, for AWS r8g (Graviton4) and r8i, and GCP C4. A shipped binary must run correctly on every target instance type: wider kernels (for example AVX-512) are selected at run time by CPU feature detection, and every kernel has a scalar fallback. A kernel set compiled in as a build baseline (lucene-rust currently builds AVX2 and NEON that way) is acceptable only if the supported-CPU floor is documented and checked at startup. | Paired scalar and accelerated correctness tests; benchmarks run on real instances of each type (not emulation). ARM CI is a manual workflow, not per PR. |
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
| SIMD | `lucene-util/src/simd.rs`: AVX2 and NEON kernels chosen by compile-time target (x86-64-v3, aarch64), AVX-512 detected at run time | Startup check of the CPU floor, whether SVE is worth adding on Graviton4, and per-instance benchmarks |
| Encryption | `lucene-store-crypto`: AES-256-GCM per file with a `KeyProvider` for BYOK | Per-index key mapping onto the hsearch key hierarchy |

Build on these crates through the existing engine adapter. Add to lucene-rust only where a row above is a confirmed gap.

## Conflicts with existing plan documents

1. **Forced refresh.** [api-compatibility](api-compatibility.md) A05 and the architecture text excluded per-request `refresh=true`. PR-07 overrides this: all three modes are supported. Forced visibility is coalesced across requests and metered, so it cannot create one tiny segment or one extra object per write. The architecture text on "per-request forced refresh" must be updated when the API task for A05 lands.
2. **Visibility target.** The SLO worksheet in [performance-scale](performance-scale.md) proposes committed-to-searchable p99 <= 250 ms and has no p50 target. PR-08 adds p50 < 500 ms and relaxes the p99 to < 5 s. PR-08 is the requirement; 250 ms may stay as an internal stretch target.
3. **Scale figure.** The plan models 1M tenants and up to 50M indexes. PR-01 is 10M indexes. The plan's figure stays a modelled upper bound.
4. **Object-durable ack p99 <= 1 s** (worksheet) and PR-08 interact: with refresh `wait_for`, ack time includes the visibility wait. The batching window must be chosen against both. This is an open tradeoff for M02.07/M03.08, not a settled number.
5. **Control plane.** The plan has maintenance, placement and copy jobs. PR-19 keeps those inside the engine but excludes any component that spawns nodes.
6. **Roles.** [architecture](architecture.md) lists API coordinator, index worker, query worker, maintenance worker and journal. PR-18 uses the user's three names. Mapping: coordinator = API/query coordinator, index = index and maintenance workers, search = query worker. The journal role stays an optional fourth role for quorum mode.
7. **API compatibility wording.** [README](README.md) and [api-compatibility](api-compatibility.md) say full OpenSearch compatibility is not required. PR-20 agrees: "as close as practical" still means the core-v1 profile, with scalability-conflicting features dropped. No change to that profile is intended.
8. **Quorum mode and PR-05.** The optional quorum backend keeps a journal tier for the unarchived tail, so PR-05 is stated for object mode and quorum mode carries its archive-lag RPO.
9. **Query paths.** The fork serves gets from the shard owner while this plan separates query replicas. No conflict, but PR-12 applies to both paths.

## Open questions for later milestones

These are not blocking. Each is settled when its milestone freezes workloads.

- Warm and cold p99 per query class (PR-31); cold-wake latency (PR-04); takeover time (PR-29).
- Which OpenSearch `knn_vector` mapping parameters map to which lucene-rust variant (PR-20, PR-22).

## Follow-up: documents that still say the old thing

This PR does not edit them. Until the API task for A05 lands, an agent reading only one of them must be pointed here: `api-compatibility.md` A05, `api-profile.json` (the refresh entry) and `architecture.md` (the "per-request forced refresh" sentence) still exclude `refresh=true`. `AGENTS.md` now says this file wins over older plan text.
