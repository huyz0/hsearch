# Engineering standards

Authority: the [core-v1 API contract](api-compatibility.md), [architecture](architecture.md), [requirements](requirements.md) and task ledger. The user's current direction is scalability-first OpenSearch-inspired partial compatibility. Earlier replacement language does not override the declared profile. Existing historical research/evidence remains provenance, not a new feature promise.

## API and specification decisions

1. Every endpoint/operator/parameter names its profile, implemented status, matching behavior, intentional differences, unsupported cases and owning task. Separate native `/v1` semantics from a thin tested compatibility adapter. Unknown options fail explicitly; absence is not an empty successful result.
2. Prefer the familiar surface when it preserves semantics and fits the workload envelope. A proposed incompatibility includes official upstream behavior, an explicit hsearch rationale, measurable resource/cost evidence required for promotion, an adaptation path and an independent reviewer. Do not infer that a feature is universally unscalable from its name or implementation language.
3. Every request and background job has finite byte/count/CPU/deadline/queue/retention budgets. Distinguish total work from concurrency, local from remote attempts and logical partitions from physical resources. Limits include retries and cancellation cleanup. Use paginated metadata and explicit large jobs; no O(total indexes) hot-path scan, timer, reader, consensus group or subscription.
4. Preserve acknowledgements, fencing, tenant/key isolation, pin/GC invariants and explicit freshness/completeness. Compatibility or lower cost cannot justify silent stale fallback, weakened durability, ambiguous rollback, invented exact counts or fabricated OpenSearch internals.
5. Pin schema/analyzer/query/profile revisions in receipts, snapshots or capability contracts where needed. Cursor/snapshot identity survives or fails explicitly across split, movement, restart and revocation. No global cross-partition transaction or instantaneous snapshot is implied by a per-partition vector.

## Proof and performance

The [profile gates](api-profile.json) and [performance ladder](performance-scale.md) distinguish unmeasured design ceilings, models, component experiments and real native/fleet evidence. Freeze workload/hardware/failure/freshness/cost profiles before accepting numerical wins. Measure RSS, queue occupancy, total/concurrent fanout, remote attempts/bytes, retained pins, maintenance debt and offered/completed/rejected load. Compare equivalent contracts and include cold/overload/recovery cases.

Exact matching claims get pinned OpenSearch/client golden fixtures for the selected subset. Intentional differences get independent native oracles and explicit migration tests. Unsupported options get negative tests. A high-cardinality or expensive query may be refused or routed to a documented job; it cannot silently change semantics to pass. The initial corpus and conformance start in M02/M06/M07, rather than waiting until M16 migration.

Keep >=90% aggregate/per-crate/changed-line production coverage requirements, native Linux x86_64/ARM64 qualification, scalar/accelerated equivalence and scoped unsafe proofs. A zero-coverable unit is N/A. Targeted checks do not close a product milestone; NOT-RUN, unavailable credentials and missing real-fleet evidence stay visible.

## Resource and performance standard

Set by Huy on 13 November 2026 and binding on every task, in every thread. Performance, cost and memory are designed in and measured, not tuned at the end. A task that touches a hot path, an I/O path or a bounded resource is not done until the checks below are met or the gap is written down with an owner.

**Design.** Every design names its downsides (what it costs in latency, requests, bytes, memory, CPU and complexity, and what it gives up) next to its benefit, and picks the simplest option that meets the measured need. Design for:

- **Throughput, cost and I/O.** Count requests, bytes moved and fsync-class waits per operation, including retries. Requests scale with nodes and bytes, never with documents, indexes or elapsed time per index.
- **Bounded memory.** Every buffer, queue, cache, batch and per-request allocation has a fixed or configured bound that is checked before allocation, so no input or load can cause out-of-memory. State the bound in the design and the failure mode when it is reached (refuse, spill or back-pressure), never unbounded growth.
- **Streaming over materializing.** Read, parse, transform and write in bounded chunks. Do not build a whole bulk, segment, result set or object in memory when a stream will do.
- **Fewer copies and fewer allocations.** Borrow instead of clone, parse in place, reuse buffers, size collections up front, and avoid per-item allocation on a hot path. A copy or allocation on a hot path needs a stated reason.
- **Write-to-search visibility.** Say when a write becomes searchable and what that costs (refresh frequency, the PUTs and segments it creates, the cache it invalidates). Visibility after a write is a measured property, not an assumption: it is part of the design and of the benchmark.
- **SIMD and hardware.** Use SIMD or a native kernel where a profile shows it pays and a scalar equivalent exists and is tested against it, on both x86-64 and ARM64.

**Implementation.** Measure before changing, change one thing, measure again, and keep the number. Profile with the tools available (callgrind, `perf`, allocation counts, request counters) and optimise what the profile shows, not what looks slow. Keep the simple version unless the measured gain justifies the added complexity. `unsafe` stays confined as already required.

**Review.** The reviewer checks, for any change on a hot or bounded path: are the bounds present and checked before allocation, is anything unbounded or materialized that could stream, are there avoidable copies or allocations, is the request or byte cost per operation counted, was a benchmark or profile run with its numbers recorded, and are the downsides stated. A missing bound or an unmeasured performance claim is a blocking finding.

**Test and benchmark.** Every bounded resource has a limit-and-one test. Every task that changes a hot path or an I/O path records a benchmark with hardware, workload, the numbers before and after, and a provisional label where they come from a shared host, and the benchmark is runnable from the repository. Where a number can be asserted (requests per call, bytes, an allocation bound), it is asserted in a test rather than only printed. A benchmark that includes writes also reports the time until the write is searchable.

**Parallel threads.** Independent tasks run in separate Claude project threads, each on its own branch. Split only tasks that do not touch the same files. Each thread edits only its own task row in `tasks.json` and its own evidence file, merges `main` before pushing, and regenerates the roadmap and milestone pages with `python3 docs/plan/tools/plan.py render` instead of editing them. Shared contracts, lockfiles and the ledger are serialized: whichever thread merges second resolves the conflict, never the first. A thread that finds work belonging to another records it under [work discovered later](README.md#work-discovered-later) rather than doing it.

## Work, review and integration

The [M03.01 development seed](archive/product-start-policy.md) separates one trait/memory-fault conformance implementation from harness-baseline measurement. After root audits the exact legitimate committed seed, accepted prerequisites and conflict-free owned paths, a normal development-only TaskStore claim may precede a separate policy merge. The full policy and nonempty code share counted final-source review, native CI and protected integration before acceptance; no policy-only packet is required. M03.02 starts only after actually accepted M03.01, milestone M03 retains whole-M01, all other gates remain, and historical M14 acceptance is unchanged. This route grants no harness/provider/runtime/product-milestone acceptance.

The [M02.02 prospective start seed](archive/native-engine-start-policy.md) removes only its whole-M01 task-start sequencing edge after accepted M19/M02.09. It is a legitimate committed development seed only after root audits exact source, prerequisites, owner and paths; it is not independently accepted policy or implementation permission. Full policy+code qualification retains original992 scope and all acceptance; whole-M02/M01 completion and every other task gate remain. Finite experiment inputs/resources must be frozen before execution without inventing M02.05 calibration or release defaults.

Use one owned task branch/worktree and resource namespace, preserve existing work and serialize shared contracts, lockfiles and ledger edits. Task-level dependencies govern start; milestone prerequisites govern completion. API-affecting work must descend from M19 and the relevant measured budget freeze. Rebase preserved M01.03 work without overwriting the revised API/spec/standards.

Run deterministic checks, then independent correctness/test review; API/routing/fencing/crypto/unsafe changes additionally receive architecture/security/scalability review. Bind reports to exact source and test evidence. Plan at most two review rounds, hard ceiling three per task; counters survive diff changes. Unresolved blockers at the ceiling require split/redesign, not passing by budget. Formal approving GitHub reviews under the user's identity require separate specific authorization; independent session reports satisfy the current workflow.

M02.10 alone has the prospective same-task completion exception in [M02.10 continuation](archive/m02-10-continuation.md): one additional candidate checkpoint, actual overall4/cumulative formal3/max4, after independently reviewed policy integration. All original BLOCKED history is retained; if this checkpoint remains blocked or incomplete, stop/split or redesign. This is not a global ceiling change or a fourth packet in any historical frozen gate.

`cargo xtask check`, `python tools/api_contract.py check` and the required hosted `ci-ok` must pass for their scope. Do not weaken tests, omit selected jobs or bypass protection. Update generated ledger/matrix files and evidence together. Public code publication and normal protected merges after independent review/checks are authorized for this project; release, deployment and paid infrastructure activation are not inferred from that authorization.
