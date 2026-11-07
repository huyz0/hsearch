# Delivery roadmap

Generated from `tasks.json`. All product capabilities are planned until cited evidence closes their tasks.

## Research first

Cost research and architecture decisions belong to M00.07, before runtime work. See [completed decisions](research/cost-decisions.md) and [executed experiments](evidence/cost-research-results.md). Implementation cost gates are inside M02/M03/M06/M07/M09; real-provider reconciliation remains in M15. There is no active M18.

Task dependencies permit parallel work; milestone prerequisites govern completion. Owners are roles to allocate, not people already assigned.

| Milestone | Status | Tasks done | Completion prerequisites |
| --- | --- | --- | --- |
| [M00: Research and delivery contract](milestones/M00.md) | done | 7/7 | None |
| [M19: Scalability-first API and engineering contract](milestones/M19.md) | done | 1/1 | M00 |
| [M01: Linux workspace and AI harness foundation](milestones/M01.md) | in_progress | 14/21 | M00 |
| [M02: API-scoped engine, catalog and cost feasibility](milestones/M02.md) | planned | 3/10 | M01 |
| [M03: Storage semantics, cost-aware formats and metadata](milestones/M03.md) | planned | 0/8 | M01 |
| [M04: Lucene Rust bridge and committed readers](milestones/M04.md) | planned | 0/6 | M02, M03 |
| [M05: mTLS, tenant policy and BYOK](milestones/M05.md) | planned | 0/7 | M03 |
| [M06: Object-durable search store on RustFS](milestones/M06.md) | planned | 0/6 | M04, M05 |
| [M07: Distributed routing and query serving](milestones/M07.md) | planned | 0/8 | M06 |
| [M08: Telemetry and AI-readable diagnostics](milestones/M08.md) | in_progress | 1/6 | M07 |
| [M09: Low-latency quorum journal module](milestones/M09.md) | planned | 0/8 | M07, M08 |
| [M10: Snapshot, export and change-stream contracts](milestones/M10.md) | planned | 0/5 | M07, M09 |
| [M11: Independent cross-cluster copy module](milestones/M11.md) | planned | 0/5 | M10 |
| [M12: Cross-cluster replication module](milestones/M12.md) | planned | 0/7 | M10 |
| [M13: Catalog foundation, elasticity and fleet scaling](milestones/M13.md) | planned | 0/5 | M07, M09, M10 |
| [M14: Performance kernels and native hardware](milestones/M14.md) | planned | 0/6 | M04, M05, M07, M09 |
| [M15: Cloud storage and KMS provider qualification](milestones/M15.md) | planned | 0/7 | M11, M12, M14 |
| [M16: Migration, compatibility and harness hardening](milestones/M16.md) | planned | 0/7 | M08, M12, M13, M15 |
| [M17: Failure, scale and release qualification](milestones/M17.md) | planned | 0/6 | M06, M08, M09, M11, M12, M13, M14, M15, M16 |

## Operating contract

A task is done only when its own definition of done, milestone release boundaries, applicable coverage/security gates, and final review are satisfied. Cite immutable run/commit artifacts through a local evidence record. A file's existence alone does not prove the result it describes.

Use `python docs/plan/tools/plan.py next` to find dependency-ready planned tasks. Reserve path/resource ownership before allocation. See [AI harness](ai-harness.md), [CI/testing](ci-testing.md), [requirements](requirements.md), and [progress](PROGRESS.md).
