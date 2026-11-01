# Native physical-line coverage projection

Status: the genuine native-report replacement and complete successor pipeline are [accepted](evidence/m01-13-integrated.md) at normally merged295bbad after final R3 and full repaired-source PR/main/manual qualification. Original M07/M12/M13 issued histories and the failed earlier manual remain frozen. M08's draft is ready to resume but unaccepted; no product runtime or release is accepted.

Historical origin: M12's initial75f30dd integration passed seven jobs in each PR41339748484/main41340616696/manual41340639807 on43 Rust lines. A new real library then exposed the LF/DA assumption below. Those observations and earlier prototype stages remain provenance; current generic acceptance comes from the final M13 evidence linked above.

## Observed counterexample and preserved history

The [retained observation](evidence/native-report-counterexample.json) binds the actual Rust1.97.1/LLVM22.1.6 production-object export from the new library. The observe file emits195 unique DA records but LF208; the observation types file emits272 DA records but LF280. The existing types file remains43/43. All baseline hits are zero because this is a genuine empty-profile denominator export. Existing parsing refuses before test execution. No coverage pass, false pass or complete event-model acceptance is claimed.

The matching JSON3.1.0 export of exactly the same objects has616/74/557 file segments and the same208/43/280 summaries. A diagnostic-only segment projection matches every actual LCOV DA coordinate and zero count:195/43/272 physical lines. This is a reproduction, not an independent oracle or accepted parser. Source digests and raw export hashes remain in the observation record. The production model is not rewritten to accommodate a collector assumption.

Pinned upstream [LCOV export](https://github.com/llvm/llvm-project/blob/llvmorg-22.1.6/llvm/tools/llvm-cov/CoverageExporterLcov.cpp) emits detailed file-line iteration separately from file summary values. [Report preparation](https://github.com/llvm/llvm-project/blob/llvmorg-22.1.6/llvm/tools/llvm-cov/CoverageReport.cpp) groups function instantiations, and [summary accumulation](https://github.com/llvm/llvm-project/blob/llvmorg-22.1.6/llvm/tools/llvm-cov/CoverageSummaryInfo.h) adds their line statistics. Their totals therefore need not equal a physical file-line union. The observed difference is consistent with that source behavior; the actual pinned-compiler fixtures must verify the replacement.

Original M01.07 remains frozen at its failed third review and b637769fe34c324d9a92e7ed519ace9917560aee. M01.12's issued rounds, all11 previous blocking keys, final clear reports and historical passing gate remain unchanged. There will be no fourth M01.12 packet. M01.13 replaces the compiler-report trust interface and projection algorithm; it does not loosen the LCOV summary check or rename an unresolved review repair. Its complete retained integration needs independent review at the final source.

## Replacement interface

Introduce a separate pure native-report projection component. The collector and merge request the pinned LLVM JSON export, explicitly using its text-format JSON mode. The component accepts only the declared llvm.coverage.json.export envelope/version3.1.0 and derives physical-line hit maps from typed file segments. The [pinned JSON exporter](https://github.com/llvm/llvm-project/blob/llvmorg-22.1.6/llvm/tools/llvm-cov/CoverageExporterJson.cpp) describes each segment as line, column, clamped nonnegative count, has-count, region-entry and gap flags.

The transformation follows the [pinned file-line semantics](https://github.com/llvm/llvm-project/blob/llvmorg-22.1.6/llvm/lib/ProfileData/Coverage/CoverageMapping.cpp): carry the previous segment across lines; distinguish skipped starts, counted entries and gaps; choose the maximum relevant entry/wrapped count for each mapped physical line. Count clamping preserves zero versus positive hits; exact invocation counts beyond signed64 are not promised. A physical line is covered when at least one relevant instance/region was executed. This is line coverage, not all-instantiations or branch coverage.

Bind the projection algorithm version, native export version and raw content digest in sealed collection/merge evidence. Unknown versions, duplicate JSON keys/files, illegal equal coordinates, bad shapes/types/flags/counts, unordered or out-of-range coordinates and exhausted budgets refuse. Source ownership, frozen byte hashes, physical bounds, profiles, binary/test inventory, production-object denominator and independent merge export remain separate checks. Summary percentages and function-accumulated totals are diagnostic metadata and never determine physical denominators or pass percentages.

The retained strict LCOV parser and its malformed-summary controls stay intact for their declared legacy protocol. Actual native LCOV DA coordinates/counts and annotated minimal Rust fixtures serve as separate projection cross-checks, including overlapping closure/function lines, generic instantiations, gaps, skipped regions, unused production bodies and failure-then-repair. No synthetic summary mismatch is silently accepted, no actual physical line is discarded, and the original Unicode changed0/1 failure remains required.

## Bounds and qualification

The new component needs pre-decode byte/depth bounds, exact types and finite file/segment/projection-work limits. Initial maxima are64MiB raw input, nesting32,10,000 files, two million segments/physical outputs and eight million transformation steps. These are refusal limits, not supported workload or speed claims. Existing whole-workload CPU/memory/disk/PID/deadline caps remain authoritative; native cost and boundary measurements must be recorded. Parsing an admitted document must not be described as an interruptible hard deadline without evidence.

Do not alter the90% aggregate/per-crate/per-file/changed production floors, selected feature/profile/target obligations, zero-line N/A audits, compiler/bootstrap/build-dir/executor/wrapper protection, direct attested nextest/Clippy/formatter dispatch, bounded precreation bundle handling or original cleanup safeguards. M01.11's accepted immutable source proof remains unchanged. Future executable observe code requires measured production coverage; its old source audit cannot be rehashed into an exemption.

Before acceptance: deterministic framing/projection/negative tests; actual pinned native failure/repair and report-coordinate oracles; complete integration qualification in both profiles; independent correctness and architecture/security review of the whole final retained tree; full native x86_64/ARM64 PR/main and explicit-base manual CI; durable exact-source gate and normal protected merge. Two review rounds are planned, hard ceiling three, with every earlier counterexample and finding retained in context. No gate is cleared merely because its review budget is exhausted.

M01.13 precedes generic M01.12/composite M01.07 acceptance and resumption of M08.01 qualification. Current43-line bootstrap passes are retained as scoped evidence; M01.09's original ARM cause and M01.08 remain open. The source-only proof, synthetic catalog model and new observation draft do not establish a running search store.


## Initial implementation observations

The [initial local record](evidence/native-projection-local.json) binds sourcea74224a and its actual native projection module hash. Complete Linux qualification passed286 component tests without skips and10 real compiler controls, plus full Rust/lint/format and TLS/object/isolation/cleanup checks. Fresh dev/release overlap fixtures each show LF5 versus4 physical lines; uncovered3/4 fails and test-only repair4/4 passes, with changed0/1 then1/1. All original policy and1/3-to3/3 controls remain.

Two fresh native projection samples measure1001 and100001 segments. The larger3,389,221-byte input took0.276s and its subprocess peaked at71,224KiB RSS in that observed environment. These are scoped sample measurements, not maximum-profile or fleet claims. The candidate additionally checks character count before making its UTF-8 byte copy. Final-source coverage, source-bound whole-suite requalification, independent reviews and hosted native qualification remain pending; no current M08 or composite acceptance is asserted.


## Retained first review and coordinate admission

Both [first-round reviews](evidence/native-projection-review-history.json) blocked candidate18cd616 on one producer-grammar issue: pinned SegmentBuilder permits an equal coordinate after an uncounted segment, including a zero-length skipped/active restore pair. The initial admission incorrectly required every location to increase. Its reports, replay and hashes remain unchanged; no counter is reset.

The repair follows the exact native invariant: locations increase, or remain equal only after an uncounted predecessor. Segment order and every record remain intact; counted-predecessor duplicates and backwards coordinates still refuse. Source-backed baseline and positive-count cases exercise decode, projection and production normalization, with the independently stated mapped-line oracle. The original optional tiny Rust diagnostic produced no usable export receipt, so no Rust reachability or whole-collector failure is claimed for this edge case. Full native qualification and separate final review of the repaired source are required.

## Historical post-merge manual fixture failure (resolved by final R3)

PR15 source1d8 normally merged as7a238d9 after finalR2clear/full7 PR/gate; its original explicit-base manual run41349817121 failed an inherited nextest positive process control: the body passed but156ms wholeprocess exceeded the synthetic100ms timeout shared with the negative seed. [Original receipt and repair contract](evidence/native-nextest-fixture-failure.json) remain retained. Same M13 task continues with its original75 base and genuine finalR3 budget, not a fresh review counter. The repair keeps the wide100ms setup as an expected-failure control, adds a deliberate300ms positive, and scopes the unchanged100ms negative only to the10second timeout_seed. Production policy, timeout/retry failure assertions and all coverage floors remain unchanged; complete repaired-source checks/reviews/PR/main/manual remain required. Generic acceptance/M08readiness stay held. This is unrelated to the unconfirmed original ARM navigation cause.

M13 is now [accepted with complete final-source review/native/event evidence](evidence/m01-13-integrated.md), resolving generic M12 acceptance and the composite M07 goal. Original b637/hard3gate, all M12/M13 issued history and the original failed manual remain frozen. M08 is ready to resume its unaccepted draft; original ARM cause/M01.09 and M01.08 remain open.
