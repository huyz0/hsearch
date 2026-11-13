# M02.02 native component integration

M02.02's bounded native embedding/lifecycle adapter and selected internal core-v1 dependency inventory are qualified through [PR31](https://github.com/huyz0/hsearch/pull/31). This accepts the declared component profile, not whole M02 or M01, a search service, HTTP/SDK compatibility, remote-provider durability, fleet, deployment or release.

## Source and native boundary

The reviewed head is `6d88535fbf668e83c990b6bba036b240f4d88fe7`, with source digest `7bf11fc33234fa51b1316d926da028ec7dbc0aa32b4d644429901c5b6159123b`. Normal protected merge `656b790b94b87507717165cf4aa1930a7fb5d9ae` has the identical tree `c34664d599d2c7dd9ad175fec58bde6bdf10166d`. Its independently derived merge-head source digest is `9226ef62e3164cecfdad90c4a7e7837993b5bdf4da888dc3cdb77ba698ea4886`. The merge used an expected-head guard and preserved source authors and history.

The Lucene Rust revision remains `46e280bc2a208544fb36ecd95932e89cd8bc5618`. The qualified boundary includes native `IndexWriter::close`, `Lock::close` and `DirectoryReader`, with the local owning wrappers and finite profile recorded in [capabilities](https://github.com/huyz0/hsearch/blob/656b790b94b87507717165cf4aa1930a7fb5d9ae/crates/engine/CAPABILITIES.md). Older candidate and R1-blocked status paragraphs there describe their original preparation phases; this record supplies current acceptance. Unsupported semantics and numeric limits are unchanged.

[EngineDirectory](https://github.com/huyz0/hsearch/blob/656b790b94b87507717165cf4aa1930a7fb5d9ae/crates/engine/src/directory.rs) observes the actual acquired native lock and its release. [Engine::close](https://github.com/huyz0/hsearch/blob/656b790b94b87507717165cf4aa1930a7fb5d9ae/crates/engine/src/adapter.rs) combines cleanup with that acknowledgment; failed or unobserved release cannot certify reusable lineage. [Snapshot](https://github.com/huyz0/hsearch/blob/656b790b94b87507717165cf4aa1930a7fb5d9ae/crates/engine/src/reader.rs) owns its native reader before its reservation, retaining quota through native teardown and refusal cleanup. Counted controls include `actual_lock_release_failure_is_returned_and_invalidates_lineage`, `reusable_close_requires_one_observed_lock_release` and `reader_teardown_keeps_its_slot_until_native_resources_are_gone`.

## Independent review and gate

Both actual R2 roles were clear, with no findings, and each explicitly resolved all four retained R1 keys. Original review base `992084caa931fc64b1589350453fde9fa0e3a765` remains authoritative. Two rounds were issued out of two planned, with a hard ceiling of three. No old report, failed context or counter was reset.

| R2 role | Canonical verdict SHA256 |
| --- | --- |
| architecture/security | `29c70e005e6c5b1a46f1d12625b51f5671b939b1f38c822a40c67ed323c4ab67` |
| correctness/tests/claims | `c5cca629710bdec00dd0b35dbf261e02f7646f201643f919cfc2ac821b37dd10` |

The normal ReviewStore gate passed at the reviewed head for packet `e8c7cc68b9efb8de4adc4ba0374274061c23fce94a5e30bb0a2567208d23ffb5`, round 2. Resolutions cover `AS-M02-02-R1-01`, `CLOSE_SUCCESS_WITHOUT_LOCK_RELEASE_ACK`, `AS-M02-02-R1-02` and `READER_SLOT_RELEASED_BEFORE_NATIVE_READER_DROP`. These are recorded independent session reports, not formal approving GitHub reviews submitted as the user.

## Observed qualification

| Actual run | Source and comparison base | Result |
| --- | --- | --- |
| [PR 42768496915](https://github.com/huyz0/hsearch/actions/runs/42768496915) | reviewed `6d88535`, base `c6f0ac0` | all 7 jobs passed, attempt 1 |
| [Manual 42768623024](https://github.com/huyz0/hsearch/actions/runs/42768623024) | reviewed `6d88535`, original base `992084c` | all 7 jobs passed, attempt 1 |
| [Main 42774598520](https://github.com/huyz0/hsearch/actions/runs/42774598520) | merge `656b790`, base `c6f0ac0` | all 7 jobs passed, attempt 1 |

Every run includes native and capped sandbox x64/ARM64 jobs and a successful `ci-ok`. Each sandbox worker passed 124 ordinary Rust tests in each dev/release profile, 477 Python tests with zero skips, 29-case capture selections per profile, 11 corpus controls and six fresh research processes of seven cases. The three Linux docs jobs also passed all 477 tests. Native jobs retain their intentional one-of-eleven diagnostic selection with ten skips separately from the complete suites. No job or run was retried or cancelled.

All twelve dev/release coverage cells passed the required aggregate, per-crate, per-file and changed-line floors of 90%. Aggregate coverage is 2052/2089 lines (98.23%). Original-base reviewed-head coverage is 1529/1566 changed lines (97.64%); PR and main coverage against `c6f0ac0` is 1189/1226 (96.98%). Zero-coverable units remain N/A. The original-base observations retain their actual reviewed-head identity; they are not relabeled as merge-head measurements.

All twelve public artifact ZIPs match GitHub SHA256 digests. For each run, the unchanged `engine_differential.package_matrix` verifier accepted both downloaded architecture packages against the actual head/tree/source digest: four native profile/architecture cells and two pinned Java architecture receipts. The postmerge packages bind the distinct merge-head digest. Context, command, actual container/image, source proof and report hashes remain in the retained evidence.

The postmerge seal is `m02-02-pr31-protected-integration-v1/artifact-index.json`, SHA256 `bfbd3bb490437ab89685c3faae68beb36114bf597cf20f6756076cd392d5f5cc`, indexing 211 retained artifacts. Its `COMPACT-INTEGRATION-EVIDENCE.json` and `POSTMERGE-QUALIFICATION-OBSERVATIONS.json` bind the reports, run/job identities, raw logs, ZIP digests, coverage contexts and package matrices. The preceding exact-head seal is `m02-02-6d885-pr31-resume-20261110-v1/artifact-index.json`, SHA256 `0a7f208b8c5a0ec63fff47d5fc81715da84ca39a48d4c8145c7ba2f14f0b52a9`, indexing 326 artifacts and a separate index of 367 execution receipts. These retained archives, rather than expiring hosted downloads alone, preserve the observations.

## Preserved history and exclusions

The source owner's Windows HOST result remains 477 run, 409 passed and 68 skipped; the focused sandbox result remains 35 run, 33 passed and two skipped, and all 13 recovery controls were skipped there. They are not native passes. The later Linux observations provide the real positives. The original eleven recovery test bodies and decorators remain intact; the final fixture repair supplied actual image/configuration observations and two additional controls without relaxing production guards.

The earlier `1ccd` runs [42564346668](https://github.com/huyz0/hsearch/actions/runs/42564346668) and [42564420250](https://github.com/huyz0/hsearch/actions/runs/42564420250) remain failed in history. Their incomplete sandbox collection and Java joins are not retrospectively accepted. The interrupted local attempt, original claims and all 39 GiB of held resources remain retained; this integration performed no resource cleanup or cancellation.

M03.01's separate [PR30 acceptance](m03-01-qualified-source.md) remains scoped to typed traits and memory/fault conformance. No unqualified M03.02 artifact/controller implementation is imported by this factual update. Remaining milestone prerequisites, native API/service work and later acceptance criteria stay explicit in the ledger.
