# Bootstrap development


M02.09's [explicit compact exporter/context4 draft](research/native-engine-coverage-profile.md) is in progress after a real64MiB runtimeexport refusal. It is unaccepted until full native/profile/coverage/review/CI qualification. Historical M13/context3 acceptance and every original regression remain preserved.

Current status: [complete M13 native-report successor accepted](evidence/m01-13-integrated.md). Context3/versioned JSON3.1 physical segments and independently rederived raw native receipts are authoritative. Earlier prototype/review/pending stages below are historical provenance; original failed gates remain frozen. M08.01's [typed contract/facade is accepted](evidence/m08-01-integrated.md); SDK/exporter/runtime remain unimplemented.

This checkout contains the integrated M01.01 workspace bootstrap and M01.02 [local sandbox](sandbox.md), and integrated M01.06 [impact CI](impact-ci.md). It is not a running search store. The [task ledger](tasks.json) retains all later product and qualification obligations. The bootstrap defines checkpoint relationships, an observation trait, a pinned engine boundary and a development entrypoint. It has no hsearch storage API, node binary, network service or persisted format.

## Run the implemented checks

Use Linux with Python 3.11+ and the exact Rust 1.97.1 toolchain in `rust-toolchain.toml`. Windows should use WSL2 or a Linux container. Native Windows/macOS support remains a later qualification task.

```sh
cargo fetch --locked
cargo xtask check
```

Prefetch needs public GitHub/crates.io access. Subsequent bootstrap Cargo checks use `--locked --offline`. `cargo xtask check` runs dependency ownership and negative controls, formatting, workspace Clippy, all debug/release tests and doctests, the production release-library build, and the existing planning/model checks. It stops at the first failing command. `PYTHON` may select another Python executable. The resource default is two Cargo build jobs; this alone is not a sandbox or host lease.

The redesigned source-bound coverage integration is an additional check, accepted through the final M13 native-report successor:

```sh
python3 tools/coverage_local.py --state-root /private/owned-leases --base <exact-ancestor-sha>
```

This freezes a clean source bundle and runs both profiles with pinned Rust/LLVM/nextest in the owned Linux sandbox. Direct native CI uses `python3 tools/coverage.py run --base <exact-ancestor-sha> --output /private/new-context`. Read the [coverage contract](coverage.md) for production-object denominators, immutable source proof, artifact checks, event bases and retained controls. Aggregate/per-crate/per-file/changed floors remain90%; a zero-coverable unit is N/A. M01.11's source primitive is accepted independently; generic collector acceptance is recorded in [final M13 integration evidence](evidence/m01-13-integrated.md). The composite M07 goal is satisfied through accepted successors; its original implementation gate remains blocked with its history preserved.

`.github/workflows/bootstrap.yml` uses the [impact planner](impact-ci.md) to select package consumers or the full native Linux x86_64/ARM64 matrix when impact is shared or uncertain. The always-run `ci-ok` rejects missing, skipped, cancelled or failed selected jobs. [M01.06 integration evidence](evidence/m01-06-integrated.md) records observed hosted selection and the enforced main-branch check requirement. Runner labels are listed in [GitHub's public-runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

## Dependency ownership

`hsearch-types` has no dependencies. `hsearch-observe` imports types only. `hsearch-engine` is the sole direct Lucene consumer, pinned to `46e280bc2a208544fb36ecd95932e89cd8bc5618`; it currently exposes whole-file inputs for compile/slice contract checks only. This is not the remote-reader spike or a search adapter. Testkit is reached only through a dev dependency and excluded from the default build and production dependency closure.

```sh
cargo xtask policy
python3 tools/workspace.py selftest
python3 tools/workspace.py inventory
```

`inventory` regenerates [the source/license inventory](dependencies.json) from locked Cargo metadata. Review its diff with every dependency change. The check examines all declared workspace edges, including dev, optional, build and target-specific edges, rejects cycles/unknown crates, binds owned edges to workspace package IDs and manifest paths, rejects unowned path/namesake packages, enforces adapter ownership and the exact resolved engine revision, and checks the selected production tree for upstream test features. The seeded unsafe graphs must fail. This is a bootstrap ownership map; future crates require explicit reviewed policy changes.

No hsearch redistribution license has been selected. Public GitHub visibility permits inspection and the platform's normal repository operations; it is not an open-source reuse grant. Upstream dependencies retain their own licenses. The pinned Lucene Rust source declares Apache-2.0. The inventory records upstream declarations and immutable sources/checksums; it is not a complete attribution bundle for a future binary distribution. No dependency source is vendored here, and no release package is produced.

## Continue delivery

Use `python3 docs/plan/tools/plan.py next` and [progress](PROGRESS.md). M01.02 and M01.06 impact CI are integrated. Owned task/recovery and bounded semantic context are integrated. M01.05 durable review is integrated; generic coverage is accepted through M13, while the frozen agent/concurrent recovery corpus remains required. The broader engine/storage/security/search/replication/provider work remains M02-M17. Keep the original acceptance criteria; bootstrap/sandbox success cannot close those milestones.

API-dependent work follows integrated [core-v1](api-compatibility.md) and [engineering standards](engineering-standards.md). M01.03 is integrated; its [runner interface](task-runner.md) and [qualification record](evidence/m01-03-integrated.md) remain distinct from M01.04 semantic navigation and later harness gates. Run `python tools/api_contract.py check` alongside workspace/plan checks.

M01.04 development interface and qualification are documented in [source-bound context and navigation](context-navigation.md). Its corpus is a development harness check, separate from product and M01.08 agent qualification.

M01.05's [independent review gate](review-gates.md) is integrated with [complete protocol qualification](evidence/m01-05-integrated.md); it does not submit formal GitHub reviews or grant merge permission. M01.09's original ARM cold-navigation cause remains open despite the integrated diagnostic increment and subsequent green native runs; M01.08 remains blocked.

M01.12's [source-proof coverage integration](coverage.md) and M01.13's native physical-report successor are accepted with complete [final evidence](evidence/m01-13-integrated.md). M01.11 is accepted independently. All original failed gates and issued review histories remain preserved; composite acceptance never clears the original M07 implementation gate.

M02.09 native experiments require the complete resolved Lucene graph (including the dev embedding crates) to omit test-util, test-support and asserting. The policy checks all-feature metadata nodes and the normal/build production tree; its original18negative controls remain, with4additional feature-leak controls. Index/search/analysis/codecs remain research dev dependencies, production store-only. Complete ordinary suites still run, followed by three additional fresh-process repetitions for each native experiment in dev and release; strict sandbox build/diagnostic workers now enforce256PIDs. No research completion follows until finalsource/nativebotharch/coverage/independentreview/fullCI acceptance.

M02.09 resource-qualified repetitions execute only through the owned capped sandbox check workers on both native architectures: three fresh runs for each dev/release profile, with the frozen source-build, CPU, RAM, PID, scratch, deadline and retained-output limits. The complete ordinary native CI tests, doctests, lint and production coverage remain required. That bare-host stage does not schedule the additional framed research repetitions and supplies no bounded-resource proof. The CI verifier must reject such frames there and require every strict capped sandbox repetition; ordinary tests cannot substitute for this matrix.
