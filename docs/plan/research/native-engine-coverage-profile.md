# Native engine coverage export profile

Status: implemented draft, NOT qualified or accepted. M02.09's frozen30f080e passed the complete x86_64 ordinary suite and three extra native repetitions/profile. Runtime LLVM JSON export then exceeded the existing64MiB command bound; the owned process group stopped and failed evidence was retained. The full debug/runtime report retained exactly64MiB before refusal. No release/ARM/coverage or research acceptance follows from that failed collection.

## Bounded representation

The [LLVM export guide](https://llvm.org/docs/CommandGuide/llvm-cov.html#export-command) documents skip-functions. The [pinned LLVM22.1.6 exporter](https://github.com/llvm/llvm-project/blob/llvmorg-22.1.6/llvm/tools/llvm-cov/CoverageExporterJson.cpp) builds file records independently and conditionally omits the root functions list. This motivates the draft; actual full-vs-compact compiler equivalence remains required.

The candidate producer uses JSON3.1 with an explicit files profile and skip-functions only. It retains all inventoried objects and all file/segment/branch/expansion/MC/DC/summary records. No filename/source filters, skipped test binaries, raised raw-byte/coordinate/work limits or reduced90percent floor are permitted. Macro expansion fields remain present. Raw report/artifact limits stay64MiB; diagnostic stdout defaults tighten to1MiB, distinct from bounded sealed reports.

The legacy full decoder stays strict by default, with every original native-format/physical-line/source regression retained. Compact decoding requires an explicit files profile and rejects full/mixed/malformed shapes. Production contexts move from3to4 and bind the profile plus the projection module digest. Old contexts refuse before native tools; no old accepted context or failed review/run is relabeled.

The physical algorithm remains llvm22-physical-line-v1. Function-accumulated summaries remain diagnostics. A new actual-compiler control compares full and compact baseline/runtime file records, totals and projected coordinates in both debug and release, using identical binaries/profiles and no filters. Existing shared-source, missing-map, denominator, artifact/shard, source proof, compiler/nextest and uncovered/repaired floor controls still run.

M02.09 remains in progress until the final source passes complete native suites, both target/profile coverage, independent correctness and architecture/security review, required hosted CI and normal protected integration. Original M01.09 ARM cause is UNCONFIRMED; M01.08 and all original product/format/runtime/provider/release gates remain open.

## Observed compact export and fixture ownership

Candidate b4c8dac passed the complete local x86 suite (307 Python tests, 11 actual compiler controls, 33 Rust tests per profile and six repeated lifecycle runs). Full and compact file records, totals and projected physical lines matched for both profiles. The real debug runtime compact export then completed at 31,341,110 bytes across 1,199 files, below the unchanged 64 MiB limit. Collection still failed: the generated fixture was included through a noncanonical parent-relative path outside the engine test directory. The existing canonical first-party path guard refused it.

The next candidate moves the byte-identical generated projection into the engine test directory and uses a local include path. No canonicalization/ownership guard, source filter, test binary, report bound or coverage floor changes. Complete new-source qualification remains required; the prior failure is retained.
