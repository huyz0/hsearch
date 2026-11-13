# Development

Everything runs with plain `cargo` and Python 3 on Linux, macOS or Windows. Toolchain is pinned in `rust-toolchain.toml`.

```sh
python3 tools/check.py                 # fmt, clippy -D warnings, cargo test, plan/API/policy checks, Python tests
cargo test --workspace                 # just the Rust tests
scripts/coverage.sh [90]               # per-crate line floor via cargo-llvm-cov (cargo install cargo-llvm-cov --locked)
python3 docs/plan/tools/plan.py next   # what to work on (product tasks first)
python3 docs/plan/tools/plan.py check  # ledger, links, generated pages (render with `plan.py render`)
python3 tools/api_contract.py check    # API contract matrix (`render` regenerates it)
```

Before changing a hot, I/O or bounded path, read the [resource and performance standard](engineering-standards.md#resource-and-performance-standard): measure first, bound every buffer, stream instead of materializing, avoid avoidable copies and allocations, keep the simpler version unless a number justifies more, and record the before/after. Independent tasks run in parallel project threads on separate branches, following the same standard's parallel-thread rules.

The capped RustFS sandbox (`cargo xtask sandbox`, `tools/sandbox.py`) and the OpenSearch reference corpus are optional, run by hand and by the manual `qualify (manual)` workflow. They are frozen: fix them if they break, do not extend them.

Historical development notes are in [archive/development-history.md](archive/development-history.md). They are not required reading.
