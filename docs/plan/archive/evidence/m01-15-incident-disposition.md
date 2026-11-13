# M01.15 ARM incident disposition: accepted policy qualification

[PR21](https://github.com/huyz0/hsearch/pull/21) integrated reviewed head `0180d482e529ca9691dd66b7b5d0a5d95cb75e42` normally as `fc6b48e3138e198d9b462fa8d5649f85e9ef6552`. Both have tree `2ccf5bc592a4ef4bf9ef2a816b8b24fdc456e66e`. The policy and its current-behavior qualification support operational closure of the composite M01.09 goal. The original ARM incident's historical cause remains **unconfirmed**. No cause-specific repair or regression proof is claimed.

## Actual review and qualification

Both actual independent R1 reports cleared the exact policy head/tree/source digest `557572f9399b1069970bb3895fd999f2458022981153e5a2ecfd8691255b5233`, with no findings. Correctness session was `01a1ab64-1401-7740-9cd1-5320979e293b`; architecture/security session was `01a1ab64-5beb-7431-b3f9-bea4a729c0d6`. Packet SHA256 is `cd22c7fab20e94edc16fdfd7d3989beb1de18ccfc8e035b0315b48ae1f1162ad`. The retained durable gate passed at R1 with exact-head Linux docs and protected `ci-ok` receipts. This was a new policy review, not a fourth M01.09 source-repair review.

| Qualification | Exact source | Observed result |
| --- | --- | --- |
| [PR run 42016188559](https://github.com/huyz0/hsearch/actions/runs/42016188559) | Reviewed `0180d482e529ca9691dd66b7b5d0a5d95cb75e42` | All seven jobs passed |
| [Main run 42021126157](https://github.com/huyz0/hsearch/actions/runs/42021126157) | Merge `fc6b48e3138e198d9b462fa8d5649f85e9ef6552` | All seven jobs passed |

Each run passed docs, native x86_64/ARM64, both capped sandboxes, planning and `ci-ok`. Decoded sandbox logs contain five fresh cold semantic sessions on each architecture: actual E0308 at line 2, complete preassertion witnesses, failing invalid Cargo input, passing restored input, independent location/isolation oracles and closed owned claims. All four corpus reports record complete five-record witnesses. Native dev/release production coverage is 523/523 on both architectures in both runs; changed production lines and the empty engine production body are N/A, not 100%.

The exact-head local Linux docs suite also passed 326 tests without skips under verified 2 CPU, 4 GiB RAM, 256 PID and 2 GiB scratch limits. Only its owned container was removed and its lease released. Live main protection required strict `ci-ok`, enforced administrators and prohibited force pushes. The normal merge used an expected-head guard; no formal approving GitHub review was submitted as the user.

[Qualification receipts](m01-16-qualification-receipts.json) retain job IDs/URLs, raw-log SHA256s, actual cold records, coverage denominators, original review hashes and the durable gate result. These are retrospective policy receipts, not evidence that the later M01.16 factual consolidation passed its own checks.

## Preserved uncertainty and gates

The original [failed run 41283307456](https://github.com/huyz0/hsearch/actions/runs/41283307456), missing routed payload, original acceptance criterion, PR9/PR20 diagnostics and all three M01.09 source-review rounds remain preserved by the [disposition contract](../incident-disposition.md). Subsequent passes cannot establish the missing historical cause. A new fallback, wrong diagnostic, incomplete witness or masked primary error fails qualification and requires a new evidence-bound incident; blind retries and weaker assertions are forbidden.

M01.08 remains unqualified. Operational incident closure removes only its incident dependency blocker; its frozen agent corpus, real session loss/recovery, isolation, escaped-defect and actual usage/cost evaluation remain separate. Whole-M01 and downstream product gates remain. M02.10's frozen source, claims, review history and resources are untouched. No runtime, provider/fleet, redistribution-license, release or deployment acceptance follows.
