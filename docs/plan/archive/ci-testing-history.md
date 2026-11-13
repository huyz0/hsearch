# Linux-first development and conditional CI

Status: M01.01 workspace, M01.02 sandbox and M01.06 impact CI are integrated; remaining harness/coverage tasks retain their own acceptance. The [scalability-first API contract](../api-compatibility.md) and [engineering standards](../engineering-standards.md) add profile/oracle/budget checks. Hosted native results and branch protection are evidence, not inferred from workflow files.

## Local environment

Pin Rust/toolchain components, dependency lockfile, RustFS image digest, test tooling and container base digest. Prefer stable Rust for release; isolate pinned nightly jobs for Miri/sanitizers/fuzzing if required. A WSL2 checkout belongs in the Linux filesystem for filesystem/build performance, not under a Windows-mounted source path by default. A Linux dev container runs non-root with declared memory/CPU/PID/tmp/disk budgets, no privileged mode and no host Docker socket in an untrusted agent sandbox.

Provide a resource-capped Compose topology with separate node volumes, RustFS bucket namespace, development CA, telemetry collector and optional test KMS. Runtime and data fixtures must be reproducible offline after the dependency/image prefetch step. A preflight checks disk reserves, tool availability, container engine health and resource leases without globally pruning unrelated caches. Failure names the next action; do not bypass the check or kill unrelated sessions.

Linux x86_64 and aarch64 run native release tests; cross compilation alone does not qualify ARM intrinsics or filesystem behavior. macOS/Windows smoke compile and CLI tests are later portability gates; WSL/container is the supported local path first. Platform-dependent I/O optimizations have a normal fallback and visible capability reporting.

## Impact planner

Always trigger a lightweight classifier/result workflow on PR, main push, manual dispatch and merge-queue events when configured. Avoid skipping the entire required workflow through top-level path filters. The classifier computes changed paths using the correct event base/head or merge base, including rename old/new paths and deletions. Fetch sufficient Git history and page API results if used. Missing base, truncated file list, parser failure, unrecognized path or inconsistent dependency graph selects the full safe suite.

Use `cargo metadata` plus a maintained manifest of non-Cargo edges: fixtures, generated code, proto/schema, engine revision, feature flags, build scripts, container assets, scripts, security policy, module conformance and documentation that controls behavior. Compute reverse dependency closure, not only crates with edited source. Build/dev/optional/target-specific edges count. A changed engine pin, lockfile, root toolchain/config, CI selector or format/schema invokes the broad relevant matrix. Unknown ownership errs toward running and flags mapping maintenance.

| Change | Required checks |
| --- | --- |
| Narrative docs/plan only | Plan/schema/link checks, docs policy; cost-model arithmetic/invalid-input controls if its code/scenarios change; no engine compile |
| Module code | Format/lint, selected crate + reverse dependencies, associated contracts/integration |
| Engine adapter or engine pin | Full adapter, query/write/parity/format/recovery matrix |
| Object/manifest/WAL/GC formats | Storage histories, corruption/crash, copy/CCR consumers and compatibility |
| Crypto/auth/identity | Security negatives, key lifecycle, copy/CCR key transfer, full applicable feature matrix |
| Query/DSL | Reference correctness, shard reduce, limits, pagination, relevant workload perf |
| Unsafe/SIMD | Scalar equivalence, per-arch runtime dispatch, Miri/ASan where applicable, affected release benchmarks |
| Harness/selector/worktree/CI | Harness selftests, selector red controls, combined integration and broader build |

The planner emits a JSON plan with reason per job, graph/hash, base/head and selected feature/target/profile. A final `ci-ok` job always runs, reads the plan, and checks **every planned job's result**. Failure/cancellation/missing result or an unexpectedly skipped job fails. A skipped unplanned job is acceptable. “All skipped” cannot pass unless the planner explicitly selected a docs-only matrix. Require `ci-ok` in hosted branch protection after verifying the setting exists. Do not report protection configured based only on a YAML file.

Selftests include docs-only change, transitive consumer, moved/deleted file, unknown directory, generated fixture, optional feature, engine pin, huge file list, missing base, cancelled job, planner failure, and a deliberately omitted non-Cargo edge. Compare selected suites against a full-suite shadow run periodically; any missed failure disables narrow selection until fixed. Incrementality improves latency without becoming a correctness oracle.

## Build caching and timing

Use sccache for cacheable rustc outputs and keyed dependency build caches, with keys including toolchain/target/features/profile/instrumentation/native dependency inputs. Do not claim sccache eliminates executable linking. Keep coverage, Miri, sanitizer and release outputs separate. Trusted main builds write shared caches; untrusted PRs cannot poison privileged/release caches. Pin third-party actions by commit, keep token permissions minimal, and never run untrusted checkout with release credentials through `pull_request_target`.

nextest runs unit/integration binaries with per-test deadlines and resource groups for serialized fault tests. Doctests and external process/Java compatibility suites remain explicit jobs. Build once/use nextest archives on compatible workers when measurement shows benefit; coverage instrumentation/object paths need a proven merge setup. Cancel obsolete PR runs by PR identity, never cancel protected release/recovery work as a side effect.

Initial timing budgets to ratify after baseline: docs p95 <=2 minutes; ordinary affected Rust PR p95 <=10 minutes warm; broad PR correctness <=25 minutes; nightly heavy qualification separate. Report queue, restore, compile, link, test, upload and critical-path times plus cache hit rate and runner cost. Budgets are goals, not exemptions: a slower mandatory failure test still runs. Optimize the measured bottleneck.

## Coverage contract: at least 90%

Enforce >=90% line coverage for all shipped first-party Rust code in aggregate and in each production crate from M01. Also enforce >=90% changed-line coverage on touched production files, and report per-file gaps so averages cannot hide a new untested subsystem. A crate with no coverable lines is reported as such, not 100%. Reused Lucene upstream coverage is separate evidence; hsearch's adapter is included locally. Add per-file enforcement if measured tooling semantics support it reliably; do not confuse a workspace threshold with a per-file threshold.

Run clean coverage with `cargo-llvm-cov`; merge only matching commit/toolchain/instrumentation reports from unit, integration and instrumented service-process tests. Include disabled optional modules in their feature jobs, then merge the declared release feature matrix. Dependency/generated code exclusions are explicit and reviewed; handwritten error paths, main wiring and unsafe wrappers cannot be excluded just to pass. Integration-only main paths need coverage instrumentation or a verified reachable-library refactor, not a blanket binary exclusion. [Coverage tool](https://github.com/taiki-e/cargo-llvm-cov).

Coverage does not prove useful tests. Critical storage/security invariants require independent reference/history oracles, corruption/fault controls, and selective mutation testing. A new gate must be observed failing on a seeded defect and passing after its removal. Preserve that control as a regression test of the gate. Report denominator and exclusions along with the number.

## API profile validation

`python tools/api_contract.py check` validates authoritative sources, alternatives, finite limits, unmeasured status, owning tasks, early dependency gates and generated matrix freshness. It runs in both full workspace checks and every CI docs job. Red controls reject fake implemented/measured claims, missing alternatives/oracles, unlimited/bool budgets, missing total-work/pin limits, non-authoritative references, removed admission/format prerequisites and stale generated content.

M02 pins actual OpenSearch/client/configuration fixtures for selected semantics. M06/M07 run matching, deliberately different and unsupported-case conformance before broader M16 migration. Runtime capabilities must reflect passing code, not the design matrix. An out-of-profile request must fail explicitly; an intentional difference uses its native oracle and adaptation test, rather than being hidden as a parity success.

## Test layers and evidence

1. Pure unit/property tests for codecs, checked arithmetic, identity, query lowering and scalar kernels.
2. Java Lucene/OpenSearch differential corpus for claimed query/analyzer/scoring/document semantics; independent client ledger for mutations and failure recovery.
3. Provider conformance on in-memory/faulting fakes and real RustFS. Real cloud tests are separate and never counted as passing when credentials are absent.
4. Deterministic state-machine/network histories, small Loom models for concurrency, and optional Turmoil adapter where it exercises production logic. Simulation does not certify real TLS/fsync/kernel behavior. [Loom](https://docs.rs/loom/latest/loom/), [Turmoil](https://docs.rs/turmoil/latest/turmoil/).
5. Real-process kill/restart/partition/disk-full/fsync-error/corruption histories. Record fs and volume assumptions; SIGKILL alone is not a power-loss test.
6. Release-profile microbench, open-loop service load, cold/warm datasets, long soak, chaos and mixed-version migrations on native x86/ARM.

PR runs bounded fixed seeds; nightly runs larger seed/shard/fuzz/mutation campaigns; release runs the frozen qualification matrix and at least a 72-hour soak. Failing seeds shrink to a reproducer and are never silently retired. Fuzz persisted parsers, protocol frames, query IR and AEAD envelopes with length limits. Unsafe targets get Miri for supported code and sanitizer coverage on native paths. Track known tool blind spots.

Every run artifact records SHA, dirty diff digest if local, tool/image/engine versions, OS/kernel/fs, machine/CPU features, feature flags, workload/seed/config hashes, raw outcomes and exclusions. PR reviews and milestone completion cite exact artifacts. Long jobs use live process/job handles for monitoring; an observation timeout is not permission to start a duplicate run.

Cost regressions use the selected profile and matched durability/freshness/failure envelope. Changes to request grouping, checkpoint pages, cache/read-ahead, retries, peer topology, key envelopes or deployment routes select request/traffic amplification checks and their affected consumers. Dedicated provider runs reconcile billable usage; ordinary PR tests use explicit counts and price fixtures labeled synthetic. A lower invoice estimate cannot pass a correctness failure, and a RustFS counter test cannot certify a cloud tariff.

M01.12 is prototyping [source-proof coverage integration](coverage.md), replacing the original blocked patch parser. Its base/event and test-migration contract is explicit; full collector qualification and independent review remain pending. M01.11 is accepted independently, and the original M01.07 failed gate remains blocked.
