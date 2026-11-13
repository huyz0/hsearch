# hsearch research and delivery plan

Authoritative product requirements: [product-requirements](product-requirements.md) (13 November 2026). Current API authority: [scalability-first core-v1](api-compatibility.md), [engineering standards](engineering-standards.md), and priority [M19](milestones/M19.md). Existing IDs/evidence remain stable; full OpenSearch compatibility is not required.

Prepared 1 November 2026 (Australia/Sydney). This is the research and planning deliverable. The directory started empty, without a Git repository or runtime. Product capabilities below are proposed until their milestone evidence proves otherwise.

Build a horizontally scalable Rust search store around [huyz0/lucene-rust](https://github.com/huyz0/lucene-rust), with immutable search data on RustFS first, then S3, GCS, and Azure Blob. Separate compute capacity from stored data, keep failure semantics explicit, and make development and operations accessible to AI through bounded, machine-readable interfaces.

## Read only what the task needs

| Question | Start here |
| --- | --- |
| What should we build, and why? | [Research findings](research/findings.md), [repository audit](research/repository-audit.md) |
| What does the system guarantee? | [Architecture](architecture.md), [requirements](requirements.md) |
| Where can another implementation plug in? | [Modules and contracts](modules.md) |
| How do encryption, mTLS, and revocation work? | [Security and BYOK](security.md) |
| How do replication and copy work? | [Replication and copy](replication-copy.md) |
| How do AI agents develop and review it? | [AI harness](ai-harness.md) |
| What should CI build and verify? | [CI and testing](ci-testing.md) |
| How do we prove scale and performance? | [Performance and scale](performance-scale.md) |
| How do we minimize API/cross-AZ costs and choose tradeoffs? | [Completed cost decisions](research/cost-decisions.md), [cost evidence](research/cost-economics.md), [operating profiles](operating-profiles.md) |
| Which cost decisions and experiments are complete now? | [Immediate decisions](research/cost-decisions.md), [executed results](archive/evidence/cost-research-results.md) |
| What is the next executable unit of work? | [Roadmap](roadmap.md), then one file in [milestones](milestones) |
| What is complete, uncertain, or blocked? | [Progress](archive/PROGRESS.md), [risk and decision register](risks.md) |
| What supports the research? | [Primary source register](research/sources.md) |

## Recommendation

Use a manifest-published immutable segment store, one ordered mutation stream per logical shard, tiered caches, and independent search replicas. Ship an object-durable log backend first. Build an optional, persistent quorum-log backend for low-latency writes; do not silently change the meaning of durability. The public API returns separate committed, applied, searchable, and archived coordinates.

An object-durable success requires the **next authorized conditional-create WAL slot of an established fenced lane**, not an arbitrary uploaded object. Research selected this one-data-PUT commit path instead of a second head CAS on every batch. Structural checkpoints and segment publication remain separate. Quorum success means a majority has synchronized the record in the configured failure domains. Object storage becomes sufficient for quorum disaster recovery only through the published archive checkpoint. Search never includes speculative operations.

The first engine spike must resolve Lucene Rust's whole-file `Input` interface. Full-file materialization is useful to establish an executable baseline, but range-native cold reads remain required for the intended architecture. See [engine integration](architecture.md#engine-integration).

Cost research is completed in M00 now: direct code/source inspection, 72 seeded batching experiments and a bounded fencing model with three failing unsafe variants. The [decisions](research/cost-decisions.md) select regional object durability, zone-local serving, fenced ordered appends, compatible dirty batching and inline initial key wrapping. Implementation and provider verification sit inside M02/M03/M06/M07/M09/M15/M16. No active M18 remains. Explicit [profiles](operating-profiles.md) expose RPO/RTO/availability changes; budget pressure cannot silently weaken an accepted contract.

## Delivery and progress rules

`tasks.json` is the authoritative task and milestone ledger. Milestone pages and `roadmap.md` are generated from it. Each task carries its goal, outputs, dependencies, validation, definition of done, owner role, status, and evidence. `PROGRESS.md` records concise phase-boundary decisions and results. No unrun test, hypothetical benchmark, or inherited README number counts as delivery evidence.

From the repository root, with Python 3.11 or newer:

```sh
python docs/plan/tools/plan.py check
python docs/plan/tools/plan.py status
python docs/plan/tools/plan.py next
python docs/plan/tools/plan.py render
python docs/plan/tools/plan.py selftest
```

These commands validate planning artifacts only. M01.01 now has a workspace bootstrap under review; [development instructions](development.md) distinguish its executable checks from the broader container harness, review service, RustFS integration and impact-based CI still planned in M01 onward. Proposed runtime commands in this plan are explicitly labeled; they are not installed interfaces.

For executable cost arithmetic, run `python docs/plan/tools/cost_model.py` (or `--json`, `--selftest`). The [input scenarios](cost-scenarios.json) are illustrative component counts and rates, not a full TCO or a current region quote. Run `python docs/plan/tools/cost_research.py --write` to reproduce the completed research experiments/model. [Former M18 IDs](milestones/M18.md) point to their new owning tasks; late cloud tests certify real implemented cost, not the first architecture decision.

Start implementation with M01 and M02. Keep engine remote-read work and the storage capability probe on the critical path. Parallel development begins after contracts and isolated worktree resources exist. Linux x86_64 and aarch64 are release targets; Windows uses WSL2 and macOS uses a Linux container before optional native qualification.
