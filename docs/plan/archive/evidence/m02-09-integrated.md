# M02.09 bounded native engine research: integration evidence

[PR19](https://github.com/huyz0/hsearch/pull/19) integrated reviewed head `95bcf0d97ec4c9ce1eca7addc40c2f29ee774f40` as merge `b19074d3022694b6180f115c875f9883c1ac9425` on 2 November 2026. Both commits have tree `a58bb0fc5d7c7c5e522611443b398a8becd3bfbb`. Its PR, main and explicit-base manual qualification support the bounded component-research acceptance reported on that PR. This additive record preserves that historical result; it does not alter the ledger, reopen the original source review or qualify a later changed tree.

## Accepted scope and observed qualification

The [accepted capability inventory](https://github.com/huyz0/hsearch/blob/95bcf0d97ec4c9ce1eca7addc40c2f29ee774f40/docs/plan/research/native-engine-capabilities.md) and [native experiments](https://github.com/huyz0/hsearch/blob/95bcf0d97ec4c9ce1eca7addc40c2f29ee774f40/crates/engine/tests/native_feasibility.rs) cover 14 selected search/aggregation cases, four analyzer cases and held-out phrase order/gap and date-boundary counterexamples. Lifecycle controls cover bounded preparation, CRUD/update/merge, frozen readers, native in-memory commit/reopen and resolved logical-ID/version replay. A deliberately unsafe serving-writer mutation exposes tentative data through NRT refresh; the private-preparation path avoids that counterexample. Simulated authority refusal in this test is not an exercised hsearch WAL/backend contract.

The four additional Lucene crates are exact-pinned engine dev dependencies. First-party production bodies and the normal engine dependency boundary remain unchanged. Final local qualification recorded 309 Linux Python cases without skips, 11 actual compiler controls, 33 Rust tests per profile, applicable all-features/doctests/lint/format/production build and six fresh capped lifecycle repetitions. Source-bound dev/release production coverage measured 523/523 lines; unchanged engine and changed-line denominators were N/A, not 100% or a waived floor.

| Qualification | Exact source | Observed result |
| --- | --- | --- |
| [PR run 41383556832](https://github.com/huyz0/hsearch/actions/runs/41383556832) | Reviewed head `95bcf0d97ec4c9ce1eca7addc40c2f29ee774f40` | All seven jobs passed, including native x86_64/ARM64, both sandboxes and `ci-ok` |
| [Main run 41386107324](https://github.com/huyz0/hsearch/actions/runs/41386107324) | Merge `b19074d3022694b6180f115c875f9883c1ac9425` | All seven jobs passed |
| [Manual run 41386195319](https://github.com/huyz0/hsearch/actions/runs/41386195319) | Same merge, explicit base `8d675dac7d48679e26b3e1e2069fd35e1382aae0` | All seven jobs passed |

Each run includes three fresh dev and three release experiment repetitions on each native architecture, executed only in the owned capped sandbox workers. Public log frames report 2 CPU, 4 GiB RAM, zero swap and 256 PIDs; all 36 experiment nextest run IDs across the three runs are distinct. The complete ordinary native tests, doctests and production coverage remain separate required checks. Their bare-host execution supplies no bounded-resource experiment proof.

The retained final correctness and architecture/security reports both clear the exact reviewed head/tree/source digest `f1e394de8c0a0613e398f69ca226c953a1bdeae6bc4f759bedd666c9b89c5d51`. Final packet SHA-256 is `178ef8b06a977d7fdc10f97a2ec59f261fdfca70cfb5c47814196348869a33ff`; retained gate SHA-256 is `42f484f4939b5975545b6ac83bcca80d7ac0f8a0830fe91620e9815d6919f639`. This was formal round 2 and conservative overall round 3, including the design precheck. Original findings, differing initial review conclusions and issued histories remain frozen; this record issues no fourth source packet or formal GitHub approval.

## Retained failures and export limits

The [coverage-profile history at the accepted head](https://github.com/huyz0/hsearch/blob/95bcf0d97ec4c9ce1eca7addc40c2f29ee774f40/docs/plan/research/native-engine-coverage-profile.md) retains the initial full real-graph LLVM export overflow at the existing 64 MiB bound. A later compact export completed, but collection correctly refused a generated fixture included through a noncanonical parent-relative path. Relocating the byte-identical fixture under engine tests repaired ownership without weakening the canonical-path guard.

The explicit files export/context4 omits only the redundant root function list, retaining every inventoried binary and file/segment/expansion record. Actual compiler controls compare full and compact file records, totals and physical projections in both profiles. This is not full real-graph export-equivalence proof: the original full real-graph export overflowed and remains a failure. Legacy decoding, source/artifact ownership, raw/work/coordinate bounds and 90% coverage floors remain strict.

Formal R1 then blocked an uncapped duplicate experiment-scheduling path in ordinary native CI. The final repair removed that duplicate path, retained the complete ordinary matrix and capped sandbox repetitions, and made the verifier refuse bare-host experiment frames and non-strict cap receipts. Earlier green local checks did not qualify the repaired source; fresh exact-source review and PR/main/manual checks did. The historical draft-status statements in the source research pages describe those earlier stages, not the later integration result recorded here.

## Delivery boundaries

Acceptance is limited to coordinator-run component research and the explicit compact coverage representation. It establishes no hsearch runtime/API, authoritative latest reads, WAL authority, crash durability, persisted format, provider/fleet qualification, delegated-agent baseline, redistribution license, release or deployment acceptance. In-memory native commit/reopen does not prove filesystem crash durability, tenant isolation or all 34 reference mutation steps. M02.02/M02.04 and other product tasks retain their original whole-M01 and task-specific gates.

At this record's base `0660c853185a3f92b25dc3ddbe924fd1b3278ff2`, the original M01.09 ARM diagnostic cause remains unconfirmed and M01.08 remains unqualified. A proposed replacement incident gate is not evidence of its integration or acceptance. M02.10's frozen candidate, claims, resources and review history are outside this record and remain untouched. Any shared ledger/status correction requires separate serialized integration.
