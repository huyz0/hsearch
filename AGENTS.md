# hsearch contributor router

The current API direction is scalability-first, OpenSearch-inspired partial compatibility. Read `docs/plan/api-compatibility.md` and `docs/plan/engineering-standards.md`; their declared profile supersedes historical broad replacement assumptions. Keep matching, deliberately different and unsupported semantics explicit, with bounded native alternatives and source-backed rationale. Full/drop-in compatibility is not required.

Read `docs/plan/PROGRESS.md` and the relevant task in `docs/plan/tasks.json` before implementation. The ledger owns task acceptance; generated milestone pages and roadmap come from `python3 docs/plan/tools/plan.py render`.

Use one branch/worktree per task. Record owner, base SHA, owned paths/resources, observed checks and remaining failures. Serialize lockfile, shared contract and ledger changes through the integrator. Never overwrite another task's work, force push, prune unrelated resources, or treat an observation timeout as a failed process.

Start with `docs/plan/development.md` for current executable commands. `cargo xtask check` runs the implemented bootstrap checks only. M01.02 sandbox and M01.06 impact CI are integrated. Remaining task/review/context/coverage gates retain separate acceptance; M01.03 is preserved unfinished while M19 takes priority. No runtime service exists yet.

- Keep `hsearch-types` independent of adapters; `hsearch-observe` depends only on types. Only `hsearch-engine` imports Lucene crates. Testkit is a dev dependency of production crates, never a normal/build dependency.
- Keep dependency, toolchain and image revisions exact. Do not enable upstream test features in production. Inventory source/licenses before introducing dependencies. No hsearch redistribution license has been selected; do not infer a reuse grant from public visibility.
- Preserve the committed/applied/searchable/archived distinction. Read `docs/plan/architecture.md` before changing authority, durability, checkpoints or readers. No speculative operations in search.
- Read `docs/plan/security.md` for encryption/authentication, `modules.md` for boundaries, `replication-copy.md` for transfer/retention, and `ci-testing.md` for validation policy.
- Bound allocation, queues, fanout, parsing, decompression and diagnostic output. No plaintext secrets in code, logs or artifacts. Unsafe is forbidden in bootstrap; a later exception needs a documented proof and matching tests.
- Product tasks need >=90% aggregate/per-crate/changed-line production line coverage and independent review of the final tree. Zero coverable lines means not applicable, not 100%. Do not lower tests, omit a required matrix, or mark unrun provider/hardware tests passed.
- Run deterministic checks before independent semantic review. Bind findings to exact code and evidence; empty findings are valid. Plan at most two rounds, with a hard ceiling of three per task. Unresolved blockers at the ceiling require split/redesign, never a pass by budget. Formal approving GitHub reviews as the user require specific authorization.

Use bounded source navigation and linked task context. Keep progress concise and separate measurements, proposals and unverified work. Runtime instructions and status claims must match observed implementation.

Run `python tools/api_contract.py check`; regenerate the matrix with `python tools/api_contract.py render`. Design gates and examples are not observed runtime evidence. Normal reviewed/check-passing public pushes and protected merges are authorized; release/deployment authorization is separate.
