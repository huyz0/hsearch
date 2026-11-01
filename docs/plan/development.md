# Bootstrap development

This checkout contains the integrated M01.01 workspace bootstrap and M01.02 [local sandbox](sandbox.md), and integrated M01.06 [impact CI](impact-ci.md). It is not a running search store. The [task ledger](tasks.json) retains all later product and qualification obligations. The bootstrap defines checkpoint relationships, an observation trait, a pinned engine boundary and a development entrypoint. It has no hsearch storage API, node binary, network service or persisted format.

## Run the implemented checks

Use Linux with Python 3.11+ and the exact Rust 1.97.1 toolchain in `rust-toolchain.toml`. Windows should use WSL2 or a Linux container. Native Windows/macOS support remains a later qualification task.

```sh
cargo fetch --locked
cargo xtask check
```

Prefetch needs public GitHub/crates.io access. Subsequent bootstrap Cargo checks use `--locked --offline`. `cargo xtask check` runs dependency ownership and negative controls, formatting, workspace Clippy, all debug/release tests and doctests, the production release-library build, and the existing planning/model checks. It stops at the first failing command. `PYTHON` may select another Python executable. The resource default is two Cargo build jobs; this alone is not a sandbox or host lease.

Coverage is an additional check:

```sh
cargo install cargo-llvm-cov --version 0.9.1 --locked
cargo llvm-cov --locked --offline --workspace --all-features \
  --exclude xtask --exclude hsearch-testkit \
  --ignore-filename-regex '/tests/' \
  --fail-under-lines 90 --json --output-path target/bootstrap-coverage.json
python3 tools/coverage_check.py target/bootstrap-coverage.json
```

The coverage denominator is current first-party production `src` code. Integration tests are in separate `tests` directories and excluded from the report. Development-only xtask and testkit are not shipped. `hsearch-types` owns the executable production lines at this stage; engine constants/reexports and the observe trait have no executable lines and are reported as N/A. Every production file present in the report and every production crate with executable lines must reach 90%. All current production lines are new in this bootstrap, so that measurement also covers this change's production lines. M01.07 still owns generic changed-line accounting, missing-instrumentation/stale-merge controls and the future service/feature coverage matrix.

`.github/workflows/bootstrap.yml` uses the [impact planner](impact-ci.md) to select package consumers or the full native Linux x86_64/ARM64 matrix when impact is shared or uncertain. The always-run `ci-ok` rejects missing, skipped, cancelled or failed selected jobs. Observed hosted selection and the main-branch check requirement remain M01.06 acceptance work. Runner labels are listed in [GitHub's public-runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

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

Use `python3 docs/plan/tools/plan.py next` and [progress](PROGRESS.md). M01.02 is integrated and M01.06 impact CI is under review. The remaining M01 tasks still require the safe resource/worktree task runner, LSP/context budgets, durable independent review protocol, impact planner, complete coverage gate and concurrent recovery qualification. The broader engine/storage/security/search/replication/provider work remains M02-M17. Keep the original acceptance criteria; bootstrap/sandbox success cannot close those milestones.

API-dependent work follows [core-v1](api-compatibility.md) and [engineering standards](engineering-standards.md). M19 is the current priority; preserve/rebase unfinished M01.03 work before resuming it. Run `python tools/api_contract.py check` alongside workspace/plan checks.
