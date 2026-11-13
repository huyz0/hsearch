# M01.18 forensic-retention redesign plan acceptance

[PR26](https://github.com/huyz0/hsearch/pull/26) integrated the reviewed plan source `3c44380f6c151cec20ac925c070a133140d1c8c5` normally as `81bae9b0bcac4f9a434565119569701bb3f1a5ac`. The merge and reviewed source have the identical tree `b0dafb8908ffe10e69741f5269a082c710279f2a`. The original review base is `5ef01ae74ecd586b1152b4b0c175db696b40d834`; the admission seed `91e87caf7dc2930e04e9d16678a0240241417179` is not a replacement review base.

This accepts only the [decomposition](../forensic-retention-redesign.md), [frozen hard3 disposition](m01-17-hard3-disposition.json) and [verbatim eleven-clause bridge](m01-18-acceptance-bridge.json). Their earlier candidate wording records the proposal stage. M01.19 host context, M01.20 opaque image and M01.21 complete operation integration remain unimplemented and require their own prerequisites, source qualification, reviews and integration. Plan acceptance grants no live FFF read, preparation, pack/map, probe/worker, unlink or resource authority.

## Actual independent review and gate

M01.18 retained two issued rounds, planned2/hard3. R1's blocked reports and source remain preserved. R2 reviewed the exact source above with both eligible nonauthor roles clear and no findings:

| Role | Actual reviewer session | Canonical report SHA256 |
| --- | --- | --- |
| Correctness/tests/claims | `01a1b429-a950-7943-bfe3-210c5545a03b` | `f009a8661572f07bf64582e02a60f21bb479b9b3206b29924f5a7a149d9bcb49` |
| Architecture/security | `01a1b42a-4c36-7651-8c9d-165f5b27d818` | `c8161547698b9ddb8fcd22feb78fd702c88cbf80f4acb3fcb0eb069170fcf016` |

Each report resolved both prior keys, `1:correctness_tests_claims:CTC-M18-R1-001` and `1:architecture_security:AS-M18-R1-01`. R2 packet SHA256 is `1067b8c36334646a92ce58e31a9b0f04eb1aeffe91bee1293a42204d52e37d40`; clean reviewed-source digest is `4e9af4b28dca2e00c8ecb70f11c12e3edb2bdd1a77038db46eb1f3486b82311a`.

The unchanged durable review gate passed with required names `ci-ok`, `git-diff-check`, `host-ci-docs`, `planning-ownership-preservation` and `source-clean-bookends`. Its canonical check-set hash is `3178d935e09550b89bb060414c9a8a3def8a783b4050362400f2e9a571e501e7`; retained gate-result SHA256 is `bee3d36481ba519a3d0c309645abf35c20cc910b3a09752573900ef3a867fb26`. Live strict `ci-ok` protection required app15368, enforced administrators and disallowed force pushes. The normal merge used the exact reviewed-head guard; no bypass or formal approving GitHub review was submitted.

## Observed PR and main qualification

Both [PR run42368573228](https://github.com/huyz0/hsearch/actions/runs/42368573228) and [push/main run42372307585](https://github.com/huyz0/hsearch/actions/runs/42372307585) passed all seven selected jobs, including `ci-ok`. Main attempt1 qualified the actual merge above.

| Job | PR job | Main job |
| --- | --- | --- |
| plan | 128391767855 | 128402775045 |
| docs | 128393894966 | 128406082309 |
| sandbox x64 | 128393895025 | 128406082391 |
| sandbox ARM64 | 128393895044 | 128406082362 |
| native x64 | 128393895068 | 128406082382 |
| native ARM64 | 128393895029 | 128406082452 |
| ci-ok | 128398223752 | 128412180544 |

Retained decoded logs bind the actual source refs. Docs and each sandbox ran326 Python tests with zero required skips. Both native architectures passed the inherited Rust qualification and dev/release production coverage523/523; changed Rust lines and the zero-coverable engine are N/A, not100%. Both sandbox smoke receipts confirm private-port and bucket isolation, no published host ports and survival of the other topology through cleanup. Mode-inapplicable steps are distinct from missing required work.

These are observations of the existing harness and plan source, not M01.19 implementation, physical-power-loss persistence or live retention evidence. Final external plan-integration receipt SHA256 is `f5c2326eaf69123998470d97635215bb25cbceb55b979434ac9005793e70d468`, seal `b84fdf90ec716aa922b5eaa2941768bdcf6413fd15706a20bed7ef7dda46f8db`. The retained ten-file main prerequisite log manifest SHA256 is `3b69106bff96e19fe76e104438349c83717b1d9ad189d370d1493b2f451db2d4`. Earlier waiting snapshots remain unchanged.

## Ownership and preserved boundaries

The original serialized same-owner plan-admission exception created only four ledger/projection paths in the seed, followed by normal claim `m01-18-be56afb7849f` for the three new plan/evidence paths. Review covered the full original-base diff. No allocator rule, immutable packet or frozen claim was retargeted.

This factual ledger/evidence update uses a separately recorded five-path administrative exception from qualified main `81bae9b0bcac4f9a434565119569701bb3f1a5ac`: tasks.json, PROGRESS.md, rendered M01/roadmap and this new evidence file. Sole writer is `01a1ad0c-a57c-7462-9adc-44dda960885a`; branch `codex/m01-18-factual-completion-v1` is isolated from the frozen claim worktrees. Authorization SHA256 is `001a69472e234788fb31f1a291d5c6d0ac7b1d47a7d926dfa6a25bd72d663927`; pre-edit ownership/preservation receipt SHA256 is `9dfa26d80f9e4bc2eeca25c4c20fd8b56c4cdfc6214f9582f005738f895eab4f`. Four shared paths overlap only the preserved same-owner M08 claim; the new evidence path had no active owner. No resource-registry absence is inferred from null task markers.

The facts above qualify PR26 only. This later bookkeeping source requires its own exact-head checks, final legal same-M18 R3, gate and integration before its ledger state can authorize successor admission. No new review is claimed here; original base, both roles, required check names, authors and issued history continue, with no R4 or reset.

M01.17 remains frozen/unaccepted at `c0a0050c600c55c0c3825aa23bb4cc6c9edd19f8`, issued3/hard3, both final reports blocked. Preserve the independent enumeration disagreement, the real eager OS-enumeration counterexample and fresh-process original-duration failure. Its original gate is never cleared; composite closure requires all accepted successors and every original clause. M01.08 remains independently unqualified; historical M01.09 ARM cause remains unconfirmed despite operational disposition. Whole-M01/product gates, older229/PAIRS/AUTH/new15, M02.10, frozen source/claim/review histories and resources remain unchanged.
