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

## Work, review and integration

The [prospective M03.01 start policy](product-start-policy.md) separates one trait/memory-fault conformance implementation from harness-baseline measurement. It requires reviewed policy integration before any product claim; M03.02 and milestone M03 retain whole-M01, and all other task gates remain. The historical M14 acceptance record is unchanged. This is a narrow source-task start profile, not harness/provider/runtime/product-milestone acceptance.

Use one owned task branch/worktree and resource namespace, preserve existing work and serialize shared contracts, lockfiles and ledger edits. Task-level dependencies govern start; milestone prerequisites govern completion. API-affecting work must descend from M19 and the relevant measured budget freeze. Rebase preserved M01.03 work without overwriting the revised API/spec/standards.

Run deterministic checks, then independent correctness/test review; API/routing/fencing/crypto/unsafe changes additionally receive architecture/security/scalability review. Bind reports to exact source and test evidence. Plan at most two review rounds, hard ceiling three per task; counters survive diff changes. Unresolved blockers at the ceiling require split/redesign, not passing by budget. Formal approving GitHub reviews under the user's identity require separate specific authorization; independent session reports satisfy the current workflow.

`cargo xtask check`, `python tools/api_contract.py check` and the required hosted `ci-ok` must pass for their scope. Do not weaken tests, omit selected jobs or bypass protection. Update generated ledger/matrix files and evidence together. Public code publication and normal protected merges after independent review/checks are authorized for this project; release, deployment and paid infrastructure activation are not inferred from that authorization.
