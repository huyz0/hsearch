# M01.02 integrated acceptance

The sandbox artifact and three independent-review rounds are recorded in [the acceptance record](m01-02-sandbox.md), with raw fresh local smoke logs. Separate correctness and security reviewers found no blockers at `d542b45f01a9568300af009a422e65209733ac5c`. Both independently exercised refusal/lease fixtures and inspected the complete Linux smoke evidence; no formal GitHub approving review was submitted under the user's identity.

[Native CI run 41264469947](https://github.com/huyz0/hsearch/actions/runs/41264469947) passed that exact reviewed head:

- x86_64 `ubuntu-24.04`, job `124724282527`.
- ARM64 `ubuntu-24.04-arm`, job `124724282341`.
- Always-run `ci-ok`, job `124725247459`.

Both native jobs passed the complete Rust suite, production coverage, immutable image build and fresh private two-store TLS/auth/S3/isolation/cleanup smoke. The required checks and unchanged base `81bde7398bec3ebb50a82f9661f50faae1a02852` were reverified immediately before integration. Full hosted logs and check JSON were preserved in the delivery workspace.

[PR #2](https://github.com/huyz0/hsearch/pull/2) was normally merged at `2026-11-01T02:21:07Z` as `f630dc8d20860b7765e7b7e021ce3bd59a87e39b`, using the user's explicit automatic normal-merge authorization. No force push, protection bypass, release, deployment or image publication occurred. This closes the M01.02 sandbox acceptance, not later harness tasks or search-runtime qualification.
