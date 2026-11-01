# AI development, review, and operations harness

Status: broader harness design, implemented in M01 and extended with each subsystem. M01.01 adds the [bootstrap check entrypoint](development.md); M01.02/M01.03/M01.06 integrate resource/worktree/recovery/impact CI. M01.04 semantic navigation is integrated; durable review and generic coverage/corpus gates retain the acceptance below. The user's existing projects inform this design; [the audit](research/repository-audit.md) records what is adopted and changed.

## Agent task lifecycle

`planned -> ready -> in_progress -> review -> done`; `blocked` records a concrete unmet dependency and next action. Readiness is computed from dependencies, not asserted by an agent. Every task packet includes task ID, goal, allowed paths, interface assumptions, base commit, acceptance/negative cases, commands, output budget, and expected evidence. The coordinator owns shared contracts and integration; implementers own bounded paths; independent reviewers inspect correctness and tests/architecture as risk warrants.

At bootstrap, initialize Git and commit the reviewed plan with provenance. Establish branch protection and CI only when repository hosting is configured; a local branch is not proof that hosted protection exists. One task uses one branch/worktree, one resource namespace and one progress file. No worker commits on main. Shared contract changes land first; downstream branches rebase before parallel implementation. Integrator serializes Cargo.lock, schema/format, root configuration and task-ledger edits to avoid predictable conflicts.

Worktree allocation records repository ID, absolute path, branch, task, base SHA, owner/session, process handles and resource leases. Docker Compose project names, RustFS bucket prefixes, ports, temp paths and diagnostic artifacts derive from the task ID plus collision-safe suffix. Never share a mutable `target` directory between builds with different instrumentation or concurrent Cargo operations. Share read-only registries, checked dependency caches and sccache where compatible. A host-wide scheduler reserves RAM/CPU/disk/PIDs for heavy jobs; coding and light inspection remain parallel.

Cleanup checks resolved paths remain within the registered worktree/resource namespace, checks dirty state and live process handles, and refuses deletion on uncertainty. A stale timestamp or lock file alone does not prove an agent died. Reclaim leases by checking the actual process/session/container state. Do not copy production credentials into worktrees. Use temporary workload credentials scoped to test prefixes.

## Progressive disclosure and token budgets

Layer 0: a small root AGENTS router with invariants and task routes. Layer 1: one task packet plus the relevant module contract, acceptance cases and current progress. Layer 2: a source outline, symbol/reference queries, bounded excerpts, selected ADR/source evidence. Layer 3: full files, complete traces and historical evidence only when a concrete question needs them. Do not preload this whole plan or every skill into each agent.

Initial budget targets, to measure in M01: always-loaded repo instructions <=2,000 tokens; ordinary task starting packet <=6,000; first review packet <=12,000 with artifact links; follow-up review gets the delta plus unresolved finding IDs. These are process budgets, not model quality guarantees. Measure actual tokenizer counts if available; clearly label byte/word estimates. Exceed a budget only with a reason and a narrower retrieval attempt recorded. Summaries keep correctness invariants, known failures, ownership and exact evidence coordinates; never compress away a negative result.

Navigation order: `rg` for unique identifiers/paths; `lsp outline` for structure; `lsp definition/reference/calls/doc` for ambiguous symbols; bounded source slices for implementation. Discover exact CLI options using `lsp schema`/help from the pinned version. Run one rust-analyzer workspace per worktree, passing the project root as supported; isolate `LSP_CLI_HOME` and cap memory. Reuse a warm process within that worktree, invalidate source/cache identities after edits, and fall back to `rg` plus `cargo check` if LSP is missing/stale. Native Windows uses WSL2/container because the inspected tool supports Unix sockets. [lsp-cli source](https://github.com/huyz0/lsp-cli/blob/cd9d492f950ab8fb8a9f54e4aea2f72cfd1aab48/README.md).

Owned `xtask task/context/worktree/evidence` metadata and lifecycle commands are integrated in M01.03; see [the interface](task-runner.md). M01.04 adds versioned bounded context requests and owned Rust semantic sessions; see [the navigation interface](context-navigation.md). Impact CI uses its separate integrated planner; there is no advertised `xtask check --affected` command. Prefer one Rust xtask/structured CLI contract over a growing collection of agent-specific shell incantations. Keep the provider/model selection configurable; do not hard-code an LLM version into acceptance logic.

## Review and architecture feedback

Run cheap deterministic checks first, then request independent semantic review on the exact staged/PR tree. A review packet names base/head SHAs, changed contract/format, expected behavior, affected reverse dependencies, test evidence and unverified areas. Reviewers examine actual code and tests, derive a concrete counterexample for a blocking finding, and cite path/line or a reproducible history. Valid outcomes include no findings. Do not require the reviewer to invent recommendations to fill a quota.

Risk routes: ordinary code gets a correctness/test review; metadata/fencing/GC, encryption, unsafe/SIMD, API compatibility and quorum changes additionally get the relevant architecture/security reviewer. Test review asks whether the test can fail for the named bug and whether its oracle is independent. CI success cannot prove the absence of an architectural error. `autoreview` may normalize deterministic/LLM findings only after required analyzer presence and Rust coverage are verified. [Tool limitations](research/repository-audit.md#lsp-and-automated-review).

Enforce at most two planned review rounds per task, with a hard ceiling of three unless the task is split and re-scoped. Persist counters independently of mutable diff hashes. Unresolved blocking findings keep the task unmergeable when the cap is hit; split or redesign, do not “pass by budget.” Verify fixes using the delta and previous finding IDs. Changes to relevant code/tests/contracts invalidate matching verdicts; prose-only progress changes do not trigger a complete rereview unless they change a claim. Bind the final artifact to the final tree hash and required check-run IDs.

The integrator rebases/merges nonconflicting tasks serially, runs affected checks on the resulting tree, and records dependency outcomes. A merge queue or synthetic integration branch verifies combined work; two separately green branches can still conflict semantically. Review PR descriptions for contract changes and evidence accuracy, and update the milestone ledger in the same integration. The actual task requester controls external publication/merge authorization; JEV decisions and LLM reviewers are not new human approval.

## Harness evaluation, not assumed productivity

Maintain a frozen representative task corpus: ordinary bug fix, broad rename, cross-crate API change, storage race, unsafe optimization, weak-test trap, stale review verdict, misleading external instruction, CI-selector omission, conflicting worktrees and agent crash. Compare baseline single-agent/full-file workflow with routed LSP/context and bounded parallel worktrees. Measure completed-correct tasks, escaped seeded defects, wall time, total tokens/cost, retries, review rounds and recovery effort. Do not claim token savings by counting only the implementer's context while ignoring reviewers.

Acceptance: seeded critical defects fail, stale/missing evidence cannot pass, parallel tasks cannot corrupt each other's files/services, coordinator can resume from durable task state after process loss, and token budgets are met on ordinary tasks without increasing escaped defects. Record samples and uncertainty; small trials establish feasibility, not universal model reliability. Agent-owned tests must have deliberately failing controls or independent reference oracles where consequential.

## AI operations interface

Product telemetry uses `tracing` spans/logs, OTel export, Prometheus metrics and optional profiles. Stable fields include request/trace ID, deployment/tenant-scoped identity, shard/incarnation, activation, operation/checkpoint, module version, error code and resource admission. Raw queries/documents, authentication material and keys are excluded by default. Tenant IDs should not become unbounded Prometheus labels; use bounded aggregates and authenticated drill-down.

Provide a versioned read-only diagnostic API and CLI, with an optional MCP facade over the **same** authorization and schema. Proposed operations:

| Diagnostic | What an AI can establish |
| --- | --- |
| capabilities and health summary | Running build, engine/module/format/CPU features, readiness reasons |
| shard state/checkpoint diff | Committed/applied/searchable/archive lag, activation and replica eligibility |
| trace lookup and bounded event timeline | Where request time or failure occurred, with artifact references |
| explain query/placement/cache | Plan, selected replicas, pruning grounds, cache misses/remote ranges, cost |
| resource snapshot | Queue bytes/counts, disk reserves, RSS/mmap/allocator, throttling, worker occupancy |
| retention graph | Roots and pin owners preventing GC; safe diagnostics do not delete them |
| copy/CCR job inspect | Phase, durable progress, key policy, lease expiry, lag and typed failure |
| crypto/identity status | Provider key-version references, policy generation, cert expiry, selected cipher backend; no secrets |
| incident bundle | Bounded manifest, redacted logs/trace refs, config hashes, build IDs and replay instructions |

Each request has tenant/role scopes, pagination, a snapshot timestamp/generation, byte/item/deadline limits, schema version and explicit truncation. Start with summaries and opaque handles; fetch one trace/page on demand. Data returned by logs/documents is untrusted evidence, never an instruction to run commands. The diagnostic service has no arbitrary shell, SQL, filesystem read, model execution, raw memory dump or secret endpoint.

Separate scoped mutation APIs for drain, restart request, job pause/resume, cache eviction and promotion. Produce a dry-run impact plan plus preconditions and audit before authorized execution. AI monitoring should detect a meaningful state change and link evidence, not spam identical status. Do not schedule external monitors in this task; the product hooks and evaluation corpus are delivery work.

An incident exercise gives an AI only operator-visible tools and asks it to diagnose injected archive stall, stale replica, revoked KMS, expired cert, GC pin leak and object corruption. Pass only if it identifies the actual cause with bounded reads, proposes a safe recovery, and does not disclose another tenant's data. Measure telemetry overhead with exporters stalled and queues saturated. Diagnostics must not take the service down when the monitoring backend fails. [OTel security guidance](https://opentelemetry.io/docs/security/handling-sensitive-data/).

Cost diagnostics add the active profile/policy generation, versioned price provenance, actual provider attempts including retries, directed byte/path attribution, budget/maintenance debt and matched cost/latency alternatives. AI must distinguish measured counts, synthetic estimates and reconciled bills, identify NAT/hairpin or per-shard polling mistakes, and propose a concrete impact plan. It cannot automatically reduce replica failure domains or freshness to fit a budget. M09.08/M16.07 extend the injected incident corpus with these cases.

The [coordinator research split](coordinator-research.md) is scoped separately from M01.08's delegated-product/parallel-agent baseline. It cannot close either original harness obligation or downstream whole-M01 product gates. Research eligibility still requires M01.14's actual accepted ledger state.
