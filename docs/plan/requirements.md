# Requirement coverage and completion evidence

The user requested deep research and an end-to-end delivery plan first. This planning package is the current deliverable. Product implementation remains explicitly planned. Requirements below preserve the full intended product, including the later module/CCR/copy/BYOK addition.

| ID | Requirement | Planned delivery and decisive evidence |
| --- | --- | --- |
| R01 | Deep evaluation of user's projects and external scalable search stores | M00; pinned repository audit, source register, comparative findings with uncertainty |
| R02 | Comprehensive plan in `docs/plan`, measurable milestone/task DoD and tracked progress | M00; task schema/DAG/evidence checks, generated milestone pages, progress ledger |
| R03 | Use unreleased `huyz0/lucene-rust`, adapt to release later | M02/M04/M16; pinned adapter, engine contract and old-format upgrade corpus |
| R04 | RustFS initially; eventual S3/GCS/Azure persistence | M03/M06/M15; actual capability/concurrency/recovery tests on each provider, not emulator equivalence |
| R05 | Horizontal compute/storage separation and massive scale | M07/M09/M13/M17; measured tiers, idle catalog model, hot splitting and bounded group resources |
| R06 | Reliable durable writes, recoverability and fresh search | M03/M04/M06/M07/M09/M17; independent receipt histories, checkpoint/reader boundaries and failure recovery |
| R07 | Modular design and explicit extensibility | M01/M03-M12/M16; dependency checks, external adapter example, capability/conformance/disabled-module tests |
| R08 | Cross-cluster replication module | M10/M12/M15/M17; separate-cluster bootstrap/stream/gap/retention/promotion histories |
| R09 | Cross-cluster copy module | M10/M11/M15/M17; resumable independent destination, blocked-source recovery and key independence |
| R10 | BYOK with pluggable key wrapping and hardware data encryption | M05/M11/M12/M14/M15; vetted block AEAD, key rotation/revocation, real provider and CPU dispatch evidence |
| R11 | Security including mTLS between nodes | M05/M07/M09/M12/M15/M17; negative identity/role/tenant tests and rotation under load |
| R12 | Optimized performance, SIMD and ARM/x64 hardware | M02/M04/M14/M17; paired scalar/accelerated correctness, real native measurements |
| R13 | Carefully justified unsafe when required | M01/M14; scoped unsafe policy, safety proofs, negative tests and sanitizer/model evidence |
| R14 | At least 90% coverage | M01 onward/M17; clean aggregated first-party production line coverage, per-crate and changed-line gates; explicit denominator |
| R15 | Fully AI-driven implementation with parallel worktrees | M01/M16; isolated resource namespaces, concurrent task corpus, crash/resume and conflict handling |
| R16 | AI reviews code, architecture and PRs | M01/M16; independently generated bounded review packets, seeded defects, commit-bound final verdicts |
| R17 | Fast incremental/conditional CI | M01/M16; reverse-dependency planner with non-Cargo edges, missing/skip/cancel controls and measured timing |
| R18 | Linux primary local and CI environment | M01/M14/M17; capped WSL/container path and native x86_64/aarch64 release matrix |
| R19 | Windows/macOS if practical | M16; platform smoke results, declared supported WSL/container path and optional native support limits |
| R20 | Tracing, logging, metrics and AI debugging/monitoring hooks | M08/M16/M17; redacted bounded diagnostic API/CLI/MCP and successful injected-incident exercises |
| R21 | Token-efficient harness with progressive disclosure and user's LSP ideas | M00/M01/M16; linked docs, bounded packets, pinned lsp-cli integration, measured full-workflow token/correctness comparison |
| R22 | End-to-end OpenSearch replacement path | M02/M07/M16/M17; explicit API/DSL/analyzer/aggregation parity matrix, migration/shadow traffic/cutover/rollback, published unsupported cases |
| R23 | Minimize S3/API and cross-AZ cost with explicit latency/reliability tradeoffs | M00.07 completed source/protocol research and experiments now; M02.07/M03.07/M03.08 implement early cost gates, followed by M06/M07.07/M08/M09.08/M15.06/M15.07/M16.07/M17 qualification. Fenced slots, compatible batching, inline wrapping, latest barriers, locality and measured usage are explicit |

An OpenSearch replacement is a migration outcome for the declared supported contract, not a claim that every JVM plugin and arbitrary script works in Rust. M02 inventories the user's required workload/client surfaces before scope is frozen; missing features remain tracked and block the matching release promise. The native store does not have to reproduce OpenSearch's internal cluster-manager APIs, but client-visible deviations must be deliberate and documented.

## Planning completion versus product completion

M00 completion proves research, executed planning experiments and a coherent delivery plan. It cannot prove product performance, cloud compatibility, security, runtime coverage or an implemented harness. M01-M17 close those requirements with executable product evidence. Initial cost research is done in M00.07; the former M18 obligations have been moved to the owning early/provider milestones and its ID is retained only as a historical mapping. Changes to targets or omitted features require a recorded decision and updated requirement mapping; an agent cannot shrink the requirement to match a green test subset.
