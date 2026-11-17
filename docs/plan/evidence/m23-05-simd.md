# M23.05 evidence: SIMD kernels for the vector paths

Provisional: every number below is from one shared 4 vCPU x86 cloud VM (Intel Xeon 2.10 GHz with AVX2, FMA and AVX-512), release build, in-memory directory, one thread, other jobs possibly running. Single runs; repeats of one row varied by about 20%. They are relative, not absolute. **There is no ARM run**: no aarch64 toolchain target or runner was available, so the NEON path is untested here. The corpus is the synthetic clustered one of M23.01 (50,000 x 128 d), queries like the data (`HS_QUERIES=near`).

## Finding

lucene-rust picks its distance and quantizer-code kernels at compile time (`cfg(target_feature = "avx2")` and `"fma"` on x86-64; NEON is in aarch64's baseline) and states that its build targets x86-64-v3. hsearch built with no `target-cpu`, so on x86-64 every vector path ran the scalar kernels. Only AVX-512 is chosen at run time (`LUCENE_RUST_SIMD`, default on where the CPU has it), and it adds nothing on top of the baseline kernels here.

## Change

- `.cargo/config.toml` sets `-C target-cpu=x86-64-v3` for every `x86_64` target. An environment `RUSTFLAGS` replaces it, so `RUSTFLAGS="-C target-cpu=x86-64"` builds for any x86-64 CPU (the CI step that forces software AES already sets its own `RUSTFLAGS`).
- `hsearch_engine::check_cpu()` (upstream's `lucene_util::cpu::check_baseline`, which probes the whole x86-64-v3 table: AVX2, FMA, BMI1/2, LZCNT, MOVBE, F16C and the rest) runs first in `hsearch-node` and refuses to start on a CPU that lacks any, naming the features, instead of dying on an illegal instruction. It is best effort, since the check is itself built for v3, and only the `hsearch-node` binary calls it (the bench examples do not; `--help` also fails first on an old CPU). The refusal path itself is tested upstream, not here.
- Tests: `an_x86_build_compiles_the_avx2_and_fma_kernels_in` (seen red: with `RUSTFLAGS="-C target-cpu=x86-64"` the test build fails to compile on its const assertions) and `the_test_machine_has_the_features_the_build_requires`.
- An environment `RUSTFLAGS` (a release pipeline, a developer's linker flag) replaces the config and silently selects the scalar kernels; the compile-time test catches it only in test builds. Host build scripts and proc macros are also built for v3, so a build host without AVX2 cannot build.

## Measured: baseline x86-64 against x86-64-v3 (queries like the data)

| Variant | Build base / v3 | Query p50 base / v3 | recall@10 |
| --- | --- | --- | --- |
| HNSW plain, beam 200 | 5.5 s / 3.9 s | 0.87 / 0.81 ms | 1.0000 both |
| HNSW bbq, beam 200 | 5.6 s / 3.9 s | 0.96 / 0.96 ms | 0.9875 both |
| IVF bbq, 16 probes | 1.29 s / 0.99 s | 1.77 / 1.16 ms | 1.0000 both |
| IVF osq4, 16 probes | 1.43 s / 1.09 s | 2.47, 2.58, 2.80 / 1.24, 1.51, 1.55 ms (three runs each) | 1.0000 both |
| disk HNSW osq4, beam 400 | 5.5 s / 4.1 s | 1.32 / 1.12 ms | 1.0000 both |

Builds are 25 to 30% faster everywhere. IVF queries are 1.5x to 2x faster (the cluster scans and rerank are distance loops). HNSW plain and bbq queries are within noise: their per-node work is small and cache-bound at this size. Recall is identical in every row. AVX-512 against AVX2 at run time (`LUCENE_RUST_SIMD=avx2` or `avx512`, v3 build): IVF bbq 1.46 against 1.47 ms, IVF osq4 1.43 against 1.63 ms, HNSW bbq 1.16 against 1.04 ms; no consistent difference, so the default stays.

## Downsides

- An x86-64-v3 binary does not run on a CPU older than Haswell (2013) or on one that hides any v3 feature from a guest; the startup check turns that into a message. A deployment that needs such a CPU builds with `RUSTFLAGS="-C target-cpu=x86-64"` and runs the scalar kernels, about 25% slower to build and up to 2x slower to scan IVF.
- Every x86 build recompiles once after this change (a cold CI cache).
- Results from a v3 and a baseline build of the same index are identical in recall here, but floating-point summation order differs between the scalar and SIMD kernels, so scores can differ in the last bits; no test compares scores across builds.

## Not done here

- ARM: NEON kernels are chosen by `cfg(target_arch = "aarch64")` upstream and need no flag; there is no run on an ARM machine. The ARM CI workflow is manual only (project decision), so this stays unrun until someone dispatches it.
- Per-kernel microbenchmarks and a run on AVX-512-only paths (popcount, VNNI) beyond the level comparison above.
- Run-time dispatch instead of a build flag: lucene-rust selects the AVX2/FMA kernels with `cfg(target_feature)`, so one binary cannot choose at start-up. A portable binary needs multiversioned kernels (each kernel compiled for a baseline and for v3 and picked once by CPU detection) as an additive upstream PR to huyz0/lucene-rust; it is not cheap and is not done here. Until then the opt-out is a rebuild with `RUSTFLAGS="-C target-cpu=x86-64"`.
