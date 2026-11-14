# Product requirement coverage

Maps each requirement in [product-requirements](product-requirements.md) (PR-01 to PR-54) to the tasks that deliver it. Tasks are listed roughly in build order, but some early ones only probe or audit (for example M03.02, M02.12); the note column says where that matters. Task ids are in [tasks.json](tasks.json), and the milestone pages are linked from the [roadmap](roadmap.md).

This table is written by hand and no gate checks it. It was verified once, on 13 November 2026, by a script that confirmed every PR id appears here and every task id exists in `tasks.json`; if you add a task that delivers a requirement, add it here.

## How the plan is ordered

1. **M03.02, then M20** is the lean path to something that runs: a node over RustFS that takes `_bulk`, answers `_search` and recovers from the object store alone. M20 is deliberately small and its formats are disposable (D19).
2. **M04 to M06** turn that slice into the durable store: engine bridge, security, the full commit protocol and the refresh modes.
3. **M21 to M23** add the product features that work on one node: documents and mappings, index management and jobs, vectors. Only M21.01 to M21.04, M21.07 and M23 can start right after M20; real-time get, `_update`, PIT, aliases, jobs and snapshots wait for parts of M06 (and M06.05 for the paged catalog), and M06.01 waits for the security work in M05.
4. **M07 to M13** make it distributed and elastic; **M24** adds extension modules once roles exist.
5. **M14 to M17** are performance, providers, migration and release.

M02 and M03 run beside M20: they freeze the numbers and formats that M20 leaves open.

## Scale and tenancy

| ID | Requirement | Tasks | Note |
| --- | --- | --- | --- |
| PR-01 | 10M indexes, skewed sizes | M02.08 (done), M13.01, M13.02, M17.03 | Large-index threshold frozen in M02.05 |
| PR-02 | Adaptive shard count, online split | M03.09, M13.03, M13.02 | Small-index lane sharing decided in M03.09 (option B, modelled; M03.08 measures) |
| PR-03 | Per-index quotas and noisy-neighbour isolation | M07.08, M13.02, M02.05 | 20% bound is proposed, confirmed in M02.05 |
| PR-04 | Cold indexes cost nothing idle | M13.01, M13.05, M02.05 | Cold-wake target set in M02.05 |

## Storage and durability

| ID | Requirement | Tasks | Note |
| --- | --- | --- | --- |
| PR-05 | Object store is the only durable tier | M20.03, M06.06, M09.07, M17.02 | Quorum mode carries its archive-lag RPO (M09) |
| PR-06 | Acknowledged write never lost | M20.03, M06.01, M06.06, M17.02 | |
| PR-07 | Durable versus searchable per request | M20.03, M06.07 | M20 has `false` and a simple `wait_for`; M06.07 adds coalesced `true` |
| PR-08 | Visibility p50 < 500 ms, p99 < 5 s | M02.05, M06.07 | Load and hardware fixed in M02.05 |
| PR-09 | S3, GCS, Azure, S3-compatible stores | M20.01, M03.02, M03.06, M15.01, M15.02, M15.03 | RustFS first, MinIO second |
| PR-10 | Format versioning, old corpus opens | M03.03, M16.02, M16.08 | Fleet format-version record specified in M03.03 |

## Cost

| ID | Requirement | Tasks | Note |
| --- | --- | --- | --- |
| PR-11 | Minimise PUT, LIST, GET; no LIST on hot paths | M02.07, M03.07, M03.08, M20.03, M20.05 | M20.05 records request counts |
| PR-12 | Minimise inter-AZ transfer | M07.07, M07.01 | Cross-zone bytes reported per query and per write |
| PR-13 | More index nodes do not raise cost per write | M07.03, M02.05 | 10% bound is proposed |
| PR-14 | Cost measured, not asserted | M02.07, M15.06, M17.03 | |
| PR-15 | Per-index cost attribution | M03.07, M08.03, M15.07 | |

## Throughput, limits and node roles

| ID | Requirement | Tasks | Note |
| --- | --- | --- | --- |
| PR-16 | Streamed concurrent `_bulk`, bounded memory | M20.04, M21.01 | |
| PR-17 | Bounded queues, admission, explicit 429 | M07.08, M06.05 | |
| PR-18 | Declared node roles | M07.09 | |
| PR-19 | Per-role metrics, drain, no control plane | M08.07, M07.09, M07.11 | |
| PR-40 | Document size up to 200 MB, streamed | M21.01, M03.03 | Format limits sized for 200 MB in M03.03 |
| PR-41 | Nested and parent-child | M21.07 | Same-shard routing; cross-shard join rejected |
| PR-42 | Mapping explosion guarded | M21.03 | |
| PR-43 | Mappings shared by content hash | M21.02, M03.03 | |
| PR-44 | Mapping evolution as in OpenSearch | M21.04 | |
| PR-45 | `_reindex` | M22.03, M10.02 | M10.02 reuses the iterator |
| PR-46 | Aliases with atomic swap | M22.02 | |
| PR-47 | Partial `_update` | M21.06, M04.03 | |
| PR-48 | Update-by-query and delete-by-query | M22.04 | |
| PR-49 | Real-time get and multi-get | M21.05, M07.12 | Owner side in M21.05; coordinator and barrier in M07.12 |
| PR-50 | Shallow snapshots | M22.05, M10.01 | M10.01 adds the quorum-mode tail |
| PR-51 | Full portable snapshots, cross-cloud restore | M11.06, M15.04 | |
| PR-52 | Zero-downtime rolling upgrades | M16.08, M16.02, M07.10, M07.11, M03.03 | |

## API and engine

| ID | Requirement | Tasks | Note |
| --- | --- | --- | --- |
| PR-20 | OpenSearch API compatibility as close as practical | M02.01 (done), M06.05, M07.04, M16.01 | Matrix rows A01 to A16 in [api-compatibility](api-compatibility.md) |
| PR-21 | Full-text search through lucene-rust | M02.02 (done), M04.01, M07.04 | |
| PR-22 | HNSW and IVF, quantization, disk variants | M23.01, M23.02, M23.03, M02.12 | |
| PR-23 | Extension modules, native C ABI and WASM | M24.01, M24.02, M24.03, M24.04, M24.05, M16.06 | Loader crate is the only `unsafe` (D18). Contract: [extension-modules](extension-modules.md) |
| PR-53 | Engine gaps fixed upstream | M04.01, M04.02, M14.03, M23.03, M02.12 | Rule applies to every task that needs an engine change |
| PR-54 | Paging, PIT, scroll | M22.01, M07.05 | |

## Security and encryption

| ID | Requirement | Tasks | Note |
| --- | --- | --- | --- |
| PR-24 | BYOK per index, default key otherwise | M05.02, M05.04 | |
| PR-25 | Re-key by same-index replication | M11.02, M11.05 | |
| PR-26 | Crypto-shredding | M05.05 | Added to M05.05's deliverables and validation |
| PR-27 | mTLS, RBAC, audit of rotation and revocation | M05.01, M05.05, M07.02 | |

## Replication, failure and availability

| ID | Requirement | Tasks | Note |
| --- | --- | --- | --- |
| PR-28 | Cross-cluster and same-index replication | M12.01, M12.03, M12.06, M12.07, M11.02 | |
| PR-29 | Fast recovery after abrupt node loss | M07.10, M02.05, M17.02 | p50 5 s, p99 15 s proposed; needs a new detection design |
| PR-35 | Graceful drain with fenced handoff | M07.11, M02.05 | 2 s pause proposed |
| PR-36 | Recovery from the object store alone | M07.10, M06.03 | |
| PR-37 | AZ failure survivable | M07.10, M17.02 | |
| PR-38 | AZ-aware placement | M07.01, M13.02 | |
| PR-39 | Avoid cross-AZ cost without losing availability | M07.07, M07.01 | |

## Hardware, correctness and operations

| ID | Requirement | Tasks | Note |
| --- | --- | --- | --- |
| PR-30 | SIMD on ARM and x86, runtime dispatch | M14.03, M14.01 | Real r8g, r8i and C4 instances; ARM CI manual |
| PR-31 | Latency targets per query class | M02.05, M14.06, M17.03 | |
| PR-32 | Fencing, differential and fault-injection tests | M03.04, M03.08, M17.02 | |
| PR-33 | Metrics, tracing, logs | M08.02, M08.03, M03.07 | |
| PR-34 | Index lifecycle, rollover, aliases | M22.02, M06.05 | |

## Decisions still open

| Decision | Where it is made | Needed before |
| --- | --- | --- |
| May small indexes share a commit lane under tenant = index | M03.09 | M03.03 (formats) |
| Recovery (PR-29), drain (PR-35), visibility (PR-08), noisy-neighbour (PR-03) and cost (PR-13, PR-39) numbers | M02.05 | M07.10, M07.11, M06.07 |
| Vector gaps in lucene-rust | M02.12 | M23 |
| WASM runtime choice and pin | M24.03 | M24.04 |
| Large-index threshold, default document, field and nesting limits | M02.05 | M13.03, M21.01, M21.03 |
