# hsearch

Object-storage-native, scalability-first search store in Rust on top of `lucene-rust`. RustFS first; S3/GCS/Azure later. The API is OpenSearch-inspired partial compatibility ([api-compatibility](docs/plan/api-compatibility.md)), not drop-in. Current state in one page: [docs/plan/CURRENT.md](docs/plan/CURRENT.md).

**Product requirements:** [docs/plan/product-requirements.md](docs/plan/product-requirements.md) is the authoritative requirement list (confirmed 13 Nov 2026). It wins over older plan text where they conflict; its conflicts section lists what it overrides.

## Find the work

```sh
python3 docs/plan/tools/plan.py next    # ready tasks, product first; harness tasks are held when over the cap
```

The ledger is `docs/plan/tasks.json` (task states, dependencies, evidence). Read the one task you are doing, not the whole plan. Everything under `docs/plan/archive/` is history: do not read it unless a task cites it.

## Commands

```sh
python3 tools/check.py        # fmt, clippy -D warnings, tests, plan/API/policy checks. Run before every commit.
scripts/coverage.sh           # cargo-llvm-cov, 90% line floor per production crate
cargo test --workspace        # Rust tests only
```

Run commands yourself and report what they printed. Never claim a test passed, a gate ran or a number was measured without having done it.

## Rules

- Only `hsearch-engine` imports Lucene crates. `hsearch-types` is independent of adapters; `hsearch-observe` depends only on types. Testkit is a dev dependency only. (`tools/workspace.py check`)
- Exact pins for dependencies, toolchain and images. No upstream test features in production. `unsafe` is forbidden.
- Keep committed / applied / searchable / archived distinct ([architecture](docs/plan/architecture.md)). No speculative work in the search path. Bound allocation, queues, fan-out, parsing and diagnostics. No plaintext secrets.
- Product tasks need >=90% line coverage per production crate and a review of the final tree. Never lower a threshold or delete a test to make a check pass.
- Reviews: one independent reviewer by default; add the architecture/security role only for security-, format- or durability-sensitive code. At most two planned rounds, hard ceiling three, then split or redesign. ([review-gates](docs/plan/review-gates.md))
- API changes: `python3 tools/api_contract.py check` (`render` regenerates the matrix). Keep matching, different and unsupported semantics explicit.
- Ship product work. The harness is frozen: do not add harness tasks or gates; harness work (tools, CI, gate code) over ~20% of recent commits holds harness tasks in `plan.py next` and shows a warning from CI `harness-budget`, which is advisory and never fails a PR. Product docs are not harness work. Add a check only for a defect that has actually shipped.

## Working agreement

One branch per task. Do not force-push or overwrite another task's work. Normal reviewed, check-passing pushes and protected merges are authorized; release/deployment is separate. No hsearch licence is selected; do not infer a reuse grant from public visibility.

Where to look: [architecture](docs/plan/architecture.md), [modules](docs/plan/modules.md), [security](docs/plan/security.md), [requirements](docs/plan/requirements.md), [CI and testing](docs/plan/ci-testing.md), [development](docs/plan/development.md).
