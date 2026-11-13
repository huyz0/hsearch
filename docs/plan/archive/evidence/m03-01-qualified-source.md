# M03.01 combined source candidate and predecessor qualification

## Accepted M03.01 status: 9 November 2026

M03.01 completed acceptance for its declared typed traits and memory/fault conformance through [PR30](https://github.com/huyz0/hsearch/pull/30). The final reviewed source was `81e31623c36a4b92382c1bbba0711c9958400698`, with tree `b01cfcb0f60587836aae01898c4adbdbf242ed0a`. It was merged through the protected branch as `5a0a0628172e42b5ccc7db5bfa5f5fbff8f3d123`, with the reviewed tree unchanged. Both final R2 roles were clear, with no M03.01 findings. The normal M03.01 gate passed, and all seven jobs in [postmerge run 42458087463](https://github.com/huyz0/hsearch/actions/runs/42458087463) completed successfully. The original formal review base remains `992084caa931fc64b1589350453fde9fa0e3a765`. Two rounds have been issued out of two planned, with a hard ceiling of three. All earlier packets and failures are retained.

Acceptance covers the ObjectBackend and MetadataStore typed traits and memory/fault conformance. It does not qualify RustFS or other providers, persistence or durability, a runtime or service, fleet, whole M01 or M03, or release. The later M03.02 artifact and controller source, actual archive transport, and final Linux, native, Docker, coverage and CI qualification remain separate and unqualified. The candidate and pending-review statements below describe their original source phases; they do not override this completed acceptance. Existing task dependencies, scope and later acceptance boundaries remain unchanged.

## Historical candidate preparation

**Historical R1 source snapshot:** the original sections below describe `53085a1d1b6ce702be967cb29aeab2bb725dc282`. References there to this candidate, pending first reviews, unchanged bodies or other-task isolation are scoped to that exact original head. Both R1 roles were clear at this head. Its245-body preservation and134-other-task/isolation assertions describe that R1 source, not the later combined tree. The input commit and original evidence remain immutable.

**Current local preparation:** [delivery-integration-v2](delivery-integration-v2.md) adopts actual protected PR29 main0a2ff358 and corrected4f development policy through normal merges; every implementation body remains exact v1. M03.01 retains one issued round/planned2/hard3 and its original992 review base; no new packet or clearance is claimed. Main postmerge qualification and complete fresh current-source qualification precede later same-task R2. The immutable v1 snapshot and earlier source observations below remain historical evidence.

Status: **pending this combined head's complete ordinary/native architecture/profile/coverage/hosted CI, both counted independent roles and protected integration**. Candidate ledger `done` describes the intended merged state only. It is not current product acceptance or an accepted prerequisite for M03.02. No future review, run, gate or merge identity is asserted.

## Implementation and limits

The candidate contains a nonempty dependency-free `hsearch-object` production crate with `ObjectBackend` and `MetadataStore` boxed-future interfaces, bounded opaque keys/versions/mutations, typed redacted outcomes, deadlines/cancellation and shared input/response/attempt admission. Conditional replacement cannot express unconditional overwrite. Retry classification borrows the unchanged request; unknown outcomes resolve the full original envelope without inventing a new identity or allowance.

The bounded nonpersistent memory double implements atomic conditional mutation, exact historical version ranges, retained accepted identities/receipts and explicit before/after-acceptance fault injection. Capacity and cancellation/deadline checks precede relevant copying/acceptance; accepted writes followed by response loss do not claim rollback. The public-trait history kit is reused by an intentionally unconditional-CAS negative against the same predicate, with fault/identity/range/budget controls and a real eight-thread CAS race. In-process contract-byte admission is not measured HTTP traffic or an allocator/RSS sandbox. Memory tokens belong to their backend/key; a real provider must qualify its own capabilities and token semantics.

Workspace/default production membership, source/license inventory and CI/coverage selection include object without a new dependency, production testkit edge or exclusion. Only engine retains Lucene ownership. No provider service, fsync/durability, persisted format, networking/HTTP/search runtime, authority, encryption, hsearch redistribution license or release/deployment acceptance is supplied.

The collector captures hashed Cargo metadata and the pinned nextest BinaryListSummary from the complete test inventory, then directly executes the frozen binary inventory with both reuse inputs, unchanged hsearch/no-retry/no-tests-fail policy and exact binary filters. It refuses postfreeze Cargo invocations, verifies source/inventory/binary/production hashes before and after shards and merge, and uses distinct shard profile environments. Existing floors, native report protocol, denominator/source proof and failure/no-profile/completeness controls remain.

## Exact predecessor evidence

All production crate/workspace/Cargo/CI/collector bodies in this candidate are byte-identical to retained `bb58438cad7f67acfc72a84079a6e5299cd7171c`, tree `26d93fa1f255f82969bc8f8ff8f03a6b699f0398`, digest `dbbe2a5b38171f6fd14235be0a51a1f9fced48a306d6f554238b94f1927bf508`. The original ancestry and full992-to-BB30-file comparison remain. New edits are confined to the assigned planning/guard/evidence paths; no unmerged M19 or M02.11 source is imported.

The exact-BB native handoff SHA256 is `855705ecdbf8349b4704a5975dfb3e546f79c58b507c373ab924ec857444cd37`; root audit SHA256 is `35c73b00f67a9c18c3b18f520ec9a1f6f010ebfd9597a5f54c1a4f4ee5f0d408`. Root verified58 native references; this integrator verified21 selected handoff/closure/context/result/control references. These are metadata/content checks of retained native execution evidence, not a new run or reviewer authentication.

Observed local Linux x86_64 used Rust1.97.1/LLVM22, pinned nextest0.9.150 at d225b1daf362c1ca8e5e048132618c1a81c57a28 and image `sha256:83b8493d4e9ad75dc46c1dcb5835a33b2a1a9a947bdac196f91e1883cf1ac6ba`. Both fresh dev/release collections passed aggregate863/863, object340/340 and changed340/340, retaining applicable per-file floors and engine0/0 N/A. Observe199/199 and types324/324 remain in the aggregate; no numeric Python floor or exclusion is introduced.

| BB profile | Raw context file SHA256 | Result file SHA256 |
| --- | --- | --- |
| dev/x86_64 | f6e33edf1ee8cac1e4390d95f05b6824a4d37f8d0dc2a31fe13f59ab2819e6d6 | adec0ff86c9a7ac9294947a1538a86ffcabe823a5b6c84fcb65fd3f5d9041281 |
| release/x86_64 | 172659925a67a44800f4a2e29547d3cccdc219f7cc99620148c630c649efdf95 | 0eb17808a5bd08eff7fdbaa856c0aa69a3244d11f6df52a1a2db38dec5fb09f4 |

Each BB profile retained12 inventoried binaries, eight nonempty shards and46 executed tests with no skips; real receipts show zero postfreeze Cargo invocations including merge. Eight native CLI trap cases exercised dual-metadata positives and missing-metadata/no-match/bad-profile negatives, retaining actual binary hashes and profile output. The Linux119-control suite passed with zero skips, including the earlier Windows-only skips. Canonical Cargo regeneration and actual39-package inventory matched committed shared files.

This is exact-BB local x64/body evidence. It is **not** a complete new combined-head ordinary suite, ARM result, hosted CI, independent review or acceptance. A prior full `cargo xtask check` at40ea81e is older-source evidence only. New source-bound contexts/checks and the required final native architecture/profile matrix remain mandatory.

## Preserved failures and lifecycle

The old40ea release context failed at the frozen-binary guard after the first shard rebuilt the engine dependency chain. The dirty-fingerprint trigger remains unproven. The old context, mismatched hashes, command receipts and failed result stay unchanged; fresh BB collections do not rehash or retroactively accept it. Initial Git identity, test compile documentation/import and Clippy failures, plus the bounded post-removal observation history, remain retained in the owner handoffs. The source-backed collector investigation identified the rebuild exposure separately from its unrecorded trigger.

Normal claims `m03-01-73859fbd3da8` and `m03-01-933df2b997f1` are both closed, their worktrees removed normally and branches/packets/evidence retained. Latest closure handoff SHA256 is `005345514d3868f5c09cc1e83f117be77daf8d65e65c6cb4da1dcd6abc7353f8`. The exact owned build-only namespace was normally cleaned and its lease released; original2CPU/4GiB/zero-swap/256PID/8GiB/1200s/1MiB limits were retained. This does not establish global orphan/liveness clearance, physical persistence or provider qualification. No new resource operation occurs in this final source preparation.

## Start rule and acceptance boundary

The [start policy](../product-start-policy.md) retains the admitted M03.01 prerequisite revision and M01.08 baseline-goal wording. Root's later prospective decision SHA256 `aac643ce3119851b8eaf14c23628e4554c2ca9c5ceacc29077481f5b9589afb2` removes only the seed-added whole-M01 M03.02 start hold. M03.02 again depends solely on **actually accepted M03.01**, exactly as main992. Candidate done cannot admit a real probe.

Whole milestone M03 still requires M01. M03.03 retains M03.01/M19/M02.08/M02.05 and its missing measured calibration. Existing behavioral controls exercise both accepted-trait probe readiness and refusal when that prerequisite is absent, reject reintroducing the retired hold, and retain whole-milestone/later-format/provider/runtime negatives. The incident model explicitly keeps traits unaccepted in memory; it never changes the ledger to fabricate readiness. All original task deliverables/DoD, source ownership, numeric caps, coverage/native/provider evidence and frozen histories remain.

Relative to main992, ledger differences are only M03.01's admitted dependency revision and proposed status/evidence, plus the already admitted M01.08 goal. M03.02's complete final task equals main992. All134 other original tasks and other ledger fields remain unchanged. M01.08 stays unqualified and the historical ARM cause unconfirmed; no whole-M01/product acceptance edge is removed.

## Ownership, review and next qualification

Assignment SHA256 `47df6e47a77b97f789c7505f20dab663ea6a792ee63716788fc192e9680aa285` names integrator `01a1ad0c-a57c-7462-9adc-44dda960885a` for24 path areas on `codex/m03-01-final-integration-v1`. Fresh main/admin/original-review base is992084caa931fc64b1589350453fde9fa0e3a765; the new branch starts at retained BB, preserving all ancestry. The pre-edit binding records exactly five same-owner M08 planning overlaps, closed M03 records, source/native/closure hashes and frozen source/resource bookends. This is administrative integration, not TaskStore allocation or immutable packet/owner retarget. Actual source fingerprints prove bytes, not ownership.

All actual code/policy/collector/coordinator contributors remain excluded from final review, including owner `01a1ab60-4bff-7752-9544-a9eab0fca10a`, this integrator, root `01a1ab4c-4786-7472-bdb7-59c35c310347` and investigator `01a1ab9f-fde6-7952-9877-efb2fce35b9c`. The same M03.01 lineage remains issued0/planned2/hard3 before dispatch. Full original992 policy+code and actual-current-main comparisons remain review scope; no hidden seed-only or collector-only review substitutes.

The external handoff binds the clean final head/tree/digest, public24-path fingerprint, actual `ReviewStore.source()`, protected BB body hashes, exact ledger delta, selected receipt hashes and actual host API/plan/render/docs/guard/Python results with Windows skips. Host controls do not establish native qualification. Complete final-source ordinary suites, native x64/ARM/profile/coverage and selected hosted CI/ci-ok, both independent roles, the normal gate, expected-head protected integration and postmerge remain pending. No publication, packet/dispatch, new native/Docker/provider/FFF run, resource admission, M19/PR29 retry or release action is performed by this handoff.
