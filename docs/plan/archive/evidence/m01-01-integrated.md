# M01.01 integration

The user explicitly authorized merging PR #1 and automatic normal merging of subsequent hsearch changes after independent multi-session review and passing required checks. No formal approving GitHub review was submitted under the user's identity. Releases and deployments remain unauthorized.

PR #1 head was reverified as `c4549eb23c45766d14376f8a8240c91097bbad96`, matching the final independent review verdict with no remaining blockers. The review fixed and rechecked the namesake dependency-identity defect; all 18 policy controls passed. The independent follow-up also passed six Python tests, plan/cost controls, 150-link validation, coverage-report validation and delta secret scan. Production Rust was unchanged from the first review.

[Native CI run 41259699908](https://github.com/huyz0/hsearch/actions/runs/41259699908) passed on GitHub-hosted `ubuntu-24.04` and `ubuntu-24.04-arm`, including their complete bootstrap suites and 43/43 executable production lines covered. Both engine/observe crates have no executable lines and remain N/A. The always-run `ci-ok` result passed. Exact source and test scope are in the [bootstrap evidence](m01-01-bootstrap.md); these checks do not qualify later runtime requirements.

Before merge, the PR was open, draft, head matched, the base was `1ac6cbb449d7193f019c34b13ad42d44c05a36f9`, and all three required checks were completed successfully. The PR was marked ready and merged normally with the exact-head guard. [PR #1](https://github.com/huyz0/hsearch/pull/1) merged at 2026-11-01 01:15:00 UTC as `81bde7398bec3ebb50a82f9661f50faae1a02852`. No force push or protection bypass was used. M01.01 is complete; M01 as a whole remains in progress.
