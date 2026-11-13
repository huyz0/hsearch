# M01.06 integrated acceptance

Separate correctness and security reviewers found no remaining blockers at `687f645f537bade1154f7704fe6f3545b59df91d`, after two recorded review rounds. The local checks and both P2 corrections are recorded in [the implementation evidence](m01-06-impact-ci.md).

Observed hosted runs passed with complete plans decoded and checked against exact heads:

| Case | Head | Run | Selected packages/jobs |
| --- | --- | --- | --- |
| Full implementation | `687f645f537bade1154f7704fe6f3545b59df91d` | [41266432061](https://github.com/huyz0/hsearch/actions/runs/41266432061) | All five packages; docs, both native Rust and both sandbox jobs |
| Narrative docs | `aceda34ea9b3760e55b638227d71e554a61856ea` | [41266481670](https://github.com/huyz0/hsearch/actions/runs/41266481670) | Docs; all native/sandbox jobs explicitly skipped |
| Types | `f4cdf7b4d9f1bbd99f20e11bdeac280ae975e4b4` | [41266764702](https://github.com/huyz0/hsearch/actions/runs/41266764702) | Types and transitive observe; both native architectures |
| Engine | `022bd5f2e2c0e84cf4027350bdcab7d5738c3430` | [41267061716](https://github.com/huyz0/hsearch/actions/runs/41267061716) | Engine only; both native architectures, actual zero-line coverage N/A |

Every selected result and `ci-ok` succeeded. Complete hosted logs, result JSON and decoded plan JSON were retained in the delivery workspace. Temporary [qualification PR #4](https://github.com/huyz0/hsearch/pull/4) was closed without merging its documentation-only fixture comments into main.

Main protection was set and independently reread: `ci-ok` from GitHub Actions app `15368` is required, branches must be up to date, administrators are included, and force pushes/deletions are disabled. The first API request was rejected with HTTP422 because it supplied both legacy and modern check fields; it applied no setting. The corrected modern `checks` payload succeeded. No protection was bypassed.

The exact reviewed head, unchanged base `f630dc8d20860b7765e7b7e021ce3bd59a87e39b`, all seven successful checks and remote protection were reverified before [PR #3](https://github.com/huyz0/hsearch/pull/3) normally merged at `2026-11-01T03:02:29Z` as `69d9163cc0813c4cbb76160166e9b2cce45e3246`. The integrated main tree then passed [run 41267554420](https://github.com/huyz0/hsearch/actions/runs/41267554420). No formal approving review under the user's identity, force push, release or deployment occurred.
