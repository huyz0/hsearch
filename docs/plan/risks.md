# Decisions, risks and unresolved evidence

## Decisions

| ID | Direction | Reason and reconsideration trigger |
| --- | --- | --- |
| D01 | Lucene Rust native adapter, full revision pin | Required engine; reconsider interface only after M02 upstream/API evidence |
| D02 | Immutable remote segments + conditional roots + ordered shard operations | Fits object durability and mutable semantics; prove publication/GC model |
| D03 | Object-durable backend first, optional quorum backend required later | Establish correctness before lower-latency persistent fleet; both have explicit contracts |
| D04 | Independent role deployment and cache affinity | Scale read/write/maintenance separately; keep authority separate from placement |
| D05 | Typed trusted Rust modules, versioned external RPC (**native-ABI stance superseded by D18**) | Extension performance without promising stable native plugin ABI |
| D06 | Application envelope BYOK, cloud SSE optional additional defense | Portable local hardware AEAD across RustFS/cloud; incurs key/cache/format lifecycle work |
| D07 | Active-passive follower-pull CCR and independent materialized copy | Clear conflict and DR contract; active-active requires new semantics |
| D08 | Linux x86_64/aarch64 release, WSL/container local path | Matches release priority; optional native ports follow qualification |
| D09 | >=90% production line coverage aggregate/per-crate/changed lines | Measurable user floor plus mutation/reference tests against weak oracles |
| D10 | Small router, progressive task contexts, bounded reviews | User tooling evidence shows full-context review loops can waste budget |
| D11 | Fail-safe dependency-aware CI and one final result | Avoid false green skips and redundant broad rebuilds |
| D12 | Read-only AI diagnostics with explicit scoped mutation APIs | AI can inspect real state while tenant/operation authority remains enforced |
| D13 | Cost-qualified compatible commit domains, dirty-only batching and complete request accounting | Payload packing alone leaves per-shard CAS/key/page floors. M03.08 must prove grouping/fencing and total savings before metadata freeze |
| D14 | Regional object baseline with zone-local serving and explicit alternative profiles | Minimize routine peer payload crossings; selected quorum/compute failure model and actual provider path determine paid traffic |
| D15 | Optimize cost only within declared latency/freshness/RPO/RTO/security constraints | Research/profiles established in M00; early component gates and M15 qualify Pareto curves/usage/bills without silent reliability downgrade |
| D16 | Ordered fenced append commit, compatible domains, inline initial wrapped DEK and amortized checkpoints | Selected now from direct fork/SlateDB inspection and executed bounded/batching models. Saves per-batch root/key-sidecar PUTs; requires strict poisoning/fencing and paid latest/tail barriers |
| D17 | Scalability-first core-v1; useful OpenSearch-inspired partial compatibility | User revision 1 November 2026: preserve bounded matching behavior; document evidence-backed differences/native alternatives and early API/cost/admission gates; no full/drop-in requirement |
| D18 | Dynamic extension modules use a versioned C-compatible ABI, in-process, with strict version checks. **Supersedes D05's no-native-ABI stance** (D05's typed trusted Rust modules and external RPC remain available as other loading models). | Huy, 13 Nov 2026. Extension performance and the Rust-or-native-binary-only rule (no JVM, no scripting plugins) outweigh the cost of a stable ABI. Needs a documented, scoped `unsafe` exception: all `unsafe` for loading (library open, symbol lookup, ABI calls) lives in one dedicated loader crate, with a written safety proof and tests; the rest of the workspace keeps `unsafe_code = "forbid"`. A module declares its ABI and engine API versions and a node refuses a mismatch. In-process loading means a module bug can take the node down, so modules are explicitly trusted, run only when enabled in node configuration, and the ABI passes bounded buffers with explicit ownership. WebAssembly modules are supported as a sandboxed option alongside native modules (confirmed by Huy 13 Nov 2026: extensions offer both choices), and are the answer where isolation matters; the runtime is a dependency that needs an exact pin and licence inventory. Reconsider if native module isolation (a separate process over RPC) becomes necessary. The loader crate, its lint exception, the ABI spec and its tests are implementation tasks and are not done by this decision. |
| D19 | Reach a first runnable slice (RustFS, engine, minimal bulk/search endpoint) in M20 before the deep features, with v0 formats that are disposable until M03.03 freezes them | Huy's ordering guidance, 13 Nov 2026 (product first, lean path). Costs some rework of the v0 log and root when M03.03 and M06.01 replace them; buys a running system to measure and to build M21 to M23 on. Reconsider if the v0 format starts to carry data anyone needs to keep: M20.05 states it does not. |

D17 is the current user-authorized API priority. The other directions retain their implementation qualification gates; they are not final protocol proofs. High-consequence choices have validation gates before production activation. No new optional user question is needed for these reversible planning decisions.

## Risk register

| ID | Risk / unresolved fact | Impact | Resolution and stop condition |
| --- | --- | --- | --- |
| K01 | Whole-file engine input cannot support efficient lazy remote reads | Cold latency, memory and cache scale | M02/M04 read-at prototype on real codecs. Do not certify remote-read milestone using materialized fallback |
| K02 | Unreleased engine API/format churn; stale orientation text | Rework and false parity assumptions | Pinned adapter/conformance corpus. Upgrade only with a passing feature/format matrix |
| K03 | Conditional object operations differ or have ambiguous outcomes | Split brain or acknowledged loss | M03 real RustFS concurrent histories and identity resolution. Fail capability probe; do not emulate CAS |
| K04 | Stale publisher survives pin/GC grace period | Data resurrection/loss | Model boundary/pin protocol, suspend/resume old writers in M06. Age-only GC cannot pass |
| K05 | Committed durability conflated with reader visibility | Speculative exposure or false freshness | M04/M07 concurrent commit/refresh failure histories, immutable reader tokens |
| K06 | One group/engine/timer per idle shard | Fleet resource explosion | M09/M13 bounded physical groups/activation/cache model; reject O(total index) per-node costs |
| K07 | Quorum library storage contract implemented incorrectly | Data loss after crash | M09 real fsync/vote/snapshot/reconfiguration fault audit, independent receipts; no memory-store shortcuts |
| K08 | Application envelope format/nonces mishandled | Confidentiality/integrity failure | M05 crypto review, vectors and corruption/retry controls before persisted production format |
| K09 | Plaintext engine scratch/cache bypasses BYOK claim | Data at rest exposure | Encrypted volume/encrypted directory policy in M05; production readiness refuses unverified scratch policy |
| K10 | Key revocation cannot be instantaneous across caches/partitions | False security promise | Bounded policy/key/reader lifetimes and fail-closed tests, explicit propagation envelope |
| K11 | CCR holds unlimited WAL or promotes without fencing | Source outage or split brain | M12 lease budgets/rebootstrap, planned fence and explicit unplanned DR authorization |
| K12 | Copy depends on source bucket/key after 'complete' | Broken migration/backup | Destination recovery with source credentials/network blocked; separate keys and storage |
| K13 | Tests mirror AI implementation or reviewer misses defect | False assurance despite coverage | Independent references/history oracle, mutation controls, harness escaped-defect evaluation |
| K14 | CI impact map misses non-Cargo dependency | False green merge | Unknown/full fallback, selector selftests and full-suite shadow comparison |
| K15 | Agent worktrees overwhelm host or leak processes | WSL failure, state interference | M01 resource leases, handle-based liveness, isolated services, safe cleanup |
| K16 | Observability leaks data or becomes a bottleneck | Tenant exposure, incident amplification | Producer redaction, cardinality/output limits, stalled-exporter tests, scoped AI access |
| K17 | Hardware/cloud access and workload budgets not yet supplied | Cannot verify absolute SLOs/provider semantics | Continue emulator/simulation work; real release claims remain NOT-RUN until authorized infrastructure exists |
| K18 | Unpinned client workload or accidental universal compatibility claim | Misleading migration and unbounded API work | M19 matrix/standards, M02 pinned selected corpus and early M06/M07 conformance; unsupported workloads get explicit adaptation reports and do not silently redefine core |
| K19 | Payload packing hides per-shard CAS, metadata-page or key-operation floor | High cost despite larger objects | M02.07/M03.08 complete operation ledger, compatible-domain experiments and traffic trace counts gate M03/M06 |
| K20 | Wrong NAT/endpoint/LB path, byte unit or payer accounting | Unexpected network bill and incorrect policy ranking | M03.07/M15.07 inspect actual infrastructure path and reconcile provider usage; DNS alone is insufficient |
| K21 | Bundling crosses crypto/tenant policy or leaves references unbounded | Security/isolation failure or GC/read amplification | Same-tenant compatible domains first; separate proof and accounting for cross-domain extents, keys, pages and pins |
| K22 | Cost pressure auto-downgrades quorum/freshness or accumulates unbounded maintenance debt | Acknowledged loss or false SLA | M09.08 preserves hard contracts and only throttles/defer/rejects permitted work; negative controls prove refusals |
| K23 | Zonal caches/prewarm or recovery traffic treated as free | Underestimated TCO or inadequate failover capacity | M07.07/M15.06 include duplicate state, N-1 capacity, cold recovery, skew and catch-up in matched-profile experiments |
| K24 | Fenced lane permits parallel appends, skips uncertainty or reclaims fence/tail keys | Acknowledged records beyond the recovered prefix | M00.07 bad models show counterexamples now; M03.08/M06 must prove full epoch/reopen/GC/provider histories before activation |
| K25 | Cached checkpoint treated as latest after removing per-batch root updates | Incorrect strong reads or hidden tail-discovery cost | M07.05 obtains an authority/tail barrier and meters control/tail reads; published snapshots never masquerade as latest |
| K26 | Zero-IO batching simulation reported as cloud latency or sustainable throughput | False performance/cost claim | Executed artifacts identify waiting/counts only; runtime one-in-flight/KMS/storage pacing and finite-trace boundaries require separate measurement |

## Implementation order and parallel lanes

Critical path: M01 harness/contracts -> M02 engine spikes + M03 storage semantics -> M04 engine bridge with M05 security -> M06 object store path -> M07 distributed search with M08 diagnostics -> M09 quorum/archive -> M10 snapshots/change stream -> M11 copy / M12 CCR -> M15 providers -> M16 migration/harness acceptance -> M17 qualification. M13 scale and M14 optimization run in their task-defined parallel lanes and also gate release.

Security M05 begins after M01/M03 contracts and gates M06 production formats. Observability M08 begins with types/contracts and instrumentation lands with every subsystem. Quorum M09 follows engine/visibility and cluster foundations; performance M14 follows measurable paths. M13 hot-shard/catalog scale requires both object and quorum scaling contracts. M16 verifies the integrated developer/migration experience. Milestone numbers group topics; the task dependency DAG is authoritative for concurrency.

Use separate path ownership for storage, engine, security, query/cluster, operations, and harness lanes. Shared protocol/format/lockfile edits serialize through the integrator. A single task should fit a bounded review; split implementation work when an unknown forces a new contract. No calendar dates or effort commitments are inferred from task counts.

M18 has been removed as an active milestone. M00.07 completes cost protocol/source research and executed experiments now. M02.07 calibrates numerical targets; M03.07/M03.08 gate metadata/WAL integration; M07.07 gates locality; M09.08 gates budget-safe quorum; M15.06/M15.07 and M16.07 supply real-provider/operator evidence before M17 release. The [old ID mapping](milestones/M18.md) preserves existing links. Later tasks validate the selected implementation, not defer the first cost decision.
