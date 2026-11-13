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

## Work discovered later

The ledger is meant to change. Anything found while building or reviewing goes into `tasks.json` through an ordinary pull request, and `plan.py check` holds it to the same rules as every other task (goal, validation, definition of done, dependencies, owner).

1. **Add it where it belongs.** A gap found inside a task becomes a new task in the milestone that owns the capability, with a `depends_on` on what it needs. A gap that no milestone owns gets a new task in the closest one, and the milestone's definition of done is amended in the same PR if the gap changes it. IDs are never reused or renumbered; a task that turns out wrong is `cancelled` with the reason, not deleted.
2. **Park it in the table below if it is not ready to be a task.** One line: what, where it was found, the milestone expected to own it. A parked item must be turned into a task, or dropped with a stated reason, before its owning milestone starts.
3. **Milestone close reconciles the plan.** Before a milestone is marked done, the gaps its evidence lists are each either an existing task (cite the ID), a new task, or an entry in [risks](risks.md) with a decision. A milestone's evidence file that names a gap with none of the three is not finished.
4. **Plan changes ride with the work that found them**, in the same PR, so the ledger and the code never disagree about what is left.

Rules 1 and 4 are checked by `plan.py check` and review; rules 2 and 3 are **reviewer-enforced conventions** (nothing parses the table or the evidence files), so a reviewer of a milestone-start or milestone-close PR must look at them. "Owner" below is where the item most plausibly lands, not a task that already says so; none of it has a task until the owning milestone is planned.

Parked items:

| Found in | Item | Expected owner |
| --- | --- | --- |
| M20.04 | Generated document ids and the bulk `create` action | M21: no task covers it yet (M21.01 is streaming and size limits, M21.06 is partial update) |
| M20.04 | Single-node scoring (`_score`) and aggregations on the HTTP node, which returns hits in engine order with a null score | No owner: M07.04 covers distributed reduction and only optional later global statistics, so this needs a task |
| M20.04 | Socket read timeouts and worker-pool exhaustion by slow clients | M07.08 (API admission) is the closest; it does not mention it yet |
| M20.04 | Index creation is two writes (root, then mapping) and a stopped writer is reopened only by the node's own request path | M06.05 (lifecycle) is the closest; it does not mention it yet |
| M20.04 | Bulk `update` action, dynamic mapping, publishing segments from the node over HTTP, TLS, and hit order without a `sort` | M21.06, M21.03, M06.02, M05 and (order) the scoring item above |

Already tracked, so not parked: segment publish and bounded recovery (M06.02, M06.03), multipart and response-loss probes (M03.10), multi-node serving and graceful drain (M07.01, M07.06, M07.11), zero-downtime rolling upgrades (M16.08), authentication, mTLS and BYOK (M05), real-time get (M21.05, M07.12), mapping evolution (M21.04), pinned views and scroll (M22.01).

The lean path to something that runs is M03.02 (RustFS probe) then M20 (object adapter, engine over the store, write path v0, minimal endpoint); M02 and M03 continue beside it and freeze what M20 deliberately left disposable. M21 to M24 add document and mapping semantics, index management and jobs, vector search and extension modules. [product-coverage](product-coverage.md) maps every PR requirement to its tasks. Keep engine remote-read work and the storage capability probe on the critical path. Parallel development begins after contracts and isolated worktree resources exist. Linux x86_64 and aarch64 are release targets; Windows uses WSL2 and macOS uses a Linux container before optional native qualification.
