# CI and testing

CI is deliberately small. Every PR and push to `main` runs:

| Job | What |
|---|---|
| `check` | `python3 tools/check.py`: fmt, clippy `-D warnings`, `cargo test --workspace --all-features`, plan/API/engine-research/dependency-policy checks, Python tests. Also writes the status summary from `tasks.json`. |
| `coverage` | `scripts/coverage.sh 90`: standard `cargo llvm-cov`, 90% line floor per production crate. Zero coverable lines is not applicable, not 100%. |
| `harness-budget` | Advisory only: warns in the job summary when tools, CI and gate code are over 20% of the last 30 commits. Never fails a PR. Product docs (requirements, architecture, decisions) are not counted as harness work. |
| `ci-ok` | Aggregates the above; the one required check. |

A small change (docs, one crate) takes the same path; there is no separate planner. Add a check only for a defect that has actually shipped.

Not on PRs: native ARM and the capped RustFS sandbox run from `qualify (manual)` (Actions, run workflow). Nothing runs on a schedule.

Performance tests follow the [resource and performance standard](engineering-standards.md#resource-and-performance-standard): a limit-and-one test for every bounded resource, benchmarks in the repository with recorded hardware, workload and before/after numbers (labelled provisional on a shared host), request and byte counts asserted where they can be, and time-to-searchable reported for any benchmark that writes. Benchmarks run by hand or from the manual workflow, not on every PR.

Test policy that stays: differential tests against Java Lucene fixtures, exact version pins, `unsafe` forbidden, no skipped provider/hardware tests reported as passed. Historical CI design is in [archive/ci-testing-history.md](archive/ci-testing-history.md).
