# M01.01 bootstrap evidence

Status: review. This is a workspace foundation, not a delivered search service or completed M01 harness.

Base: `1ac6cbb449d7193f019c34b13ad42d44c05a36f9`, branch `codex/m01-workspace`, isolated task worktree. The integrator owns the lockfile, root configuration and task-ledger changes. The original planning directory is preserved.

## Implemented

- Five-crate Cargo workspace: types, observe, engine, testkit, xtask. Default builds include production libraries only; the engine alone imports Lucene and testkit is a dev dependency.
- Rust 1.97.1, Cargo.lock and Lucene revision `46e280bc2a208544fb36ecd95932e89cd8bc5618` are pinned. [Resolved sources/licenses](../../dependencies.json) include exact registry checksums and the engine source revision. Hsearch has no selected redistribution license; no dependency source or binary bundle is published.
- `Sequence` cannot wrap on checked advancement. `Progress` rejects applied beyond committed, searchable beyond applied and archived beyond committed while allowing archival ahead of application. Callers still own activation/shard identity and authority checks. No wire/disk protocol is ratified.
- Observe is a payload-free trait. Engine exposes pinned whole-file input types and has only compile/slice smoke tests. Testkit provides compile-time Send/Sync assertions. These are not implementations of the remote-reader spike, telemetry service or operational harness.
- Dependency checks inspect declared normal/dev/build/optional/target-specific edges, refuse unowned crates/cycles, isolate Lucene imports and reject upstream test features in the selected production tree. Unsafe graph controls are tested.
- Bootstrap CI is configured for native Linux x86_64 and ARM64 with an always-run `ci-ok` result job. It runs the full bootstrap suite; impact selection remains M01.06. No hosted branch-protection setting is claimed.

## Observed local checks

Ran in the existing linux/amd64 image `sha256:a8f1c388b4e0d5cbf1d1b6bb6a1a0184ee70c9597b8743aa8038d8bdfc6e32f1` as UID/GID 1000, capped at 2 CPUs / 4 GiB / 512 PIDs, read-only root, no capabilities or privilege escalation, with only this task's source/cache writable. Prefetch used public network access; validation had networking disabled. Tool versions: Rust/Cargo 1.97.1, Python 3.14.4, cargo-llvm-cov 0.9.1. This use of an existing image is not the reproducible dev-image deliverable in M01.02.

`cargo xtask check` passed formatting, warnings-as-errors Clippy on every workspace target/feature, all five Rust integration tests in debug and release, explicit doctest invocations (zero examples currently), the production release-library build, dependency controls, plan graph/link/render checks, 16 plan negative controls, 8 cost arithmetic and 7 invalid-input controls, and six Python tests. The Python research test reproduces all 72 saved experiments and both research artifacts. [Observed Linux output](m01-01-linux-check.txt) preserves the run.

The production dependency tree contains neither testkit nor the upstream test-util/test-support features. The policy also rejects external dependencies in the foundation crates, not just workspace edges. CLI help succeeds; missing/unknown commands must fail. Final policy/CLI verification follows the full run in the log.

Coverage used the separately instrumented target directory and excluded integration-test source, testkit and development-only xtask. `hsearch-types` covered **43/43 production lines (100%)**. Observe and engine have no executable production lines in this bootstrap and are N/A, not 100%. Per-file and per-crate report checks passed. All production lines are newly introduced here. The generic future changed-line, binary-presence, clean-profile merge and feature-matrix gates remain M01.07 obligations. Coverage controls reject a below-threshold file, missing report, duplicate report and malformed counts.

Initial runs rejected undocumented integration-test crates and a development link escaping the plan's existing boundary. Test crate documentation was added and the development/inventory files moved into `docs/plan`; no lint, test or path boundary was weakened. Static inspection tightened an omitted external-dependency ownership check before final review.

## Remaining acceptance

The first independent semantic/security/test review at `fdcd4b5b43de4f57137d6a5d0b0d8f289833aa95` found a P2 ownership-gate defect: an unowned dependency named `hsearch-types` could satisfy a name-only rule and be mislabeled as workspace source. This revision binds declared paths and resolved package IDs to owned manifests, rejects unowned path/namesake packages, and refuses the misleading inventory. Path/registry substitution, inactive declared edges, resolved alias substitution, member-path substitution and inventory counterexamples were added. The [complete offline Linux suite after this fix](m01-01-review-fix-check.txt) passed, including all **18 dependency-policy controls**; Rust production sources and the 43/43-line coverage report are unchanged. Follow-up review must bind to the corrected commit before publication.

Hosted native ARM64 and x86_64 CI results remain pending until observed; local cross-compilation is not substituted. M01.01 remains `review`, and broader M01.02-M17 runtime/security/provider/hardware/scale/soak/billing gates remain open. No release or deployment is performed.
