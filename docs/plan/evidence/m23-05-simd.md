# M23.05 evidence: SIMD kernels for the vector paths

Provisional: every number below is from one shared 4 vCPU x86 cloud VM (Intel Xeon 2.10 GHz with AVX2, FMA and AVX-512), release build, in-memory directory, one thread, other jobs possibly running. Single runs; repeats of one row varied by about 20%. They are relative, not absolute. **There is no ARM run**: no aarch64 toolchain target or runner was available, so the NEON path is untested here. The corpus is the synthetic clustered one of M23.01 (50,000 x 128 d), queries like the data (`HS_QUERIES=near`).

## Finding

lucene-rust picks its distance and quantizer-code kernels at compile time (`cfg(target_feature = "avx2")` and `"fma"` on x86-64; NEON is in aarch64's baseline) and states that its build targets x86-64-v3. hsearch built with no `target-cpu`, so on x86-64 every vector path ran the scalar kernels. Only AVX-512 is chosen at run time (`LUCENE_RUST_SIMD`, default on where the CPU has it), and it adds nothing on top of the baseline kernels here.

## Change

- The default build is **unchanged and portable**. `scripts/build-v3.sh` builds with `-C target-cpu=x86-64-v3` (it keeps any `RUSTFLAGS` you set, adds the flag on x86-64 and is a plain build elsewhere; with no arguments it runs `cargo build --release`) for the release artifacts that will run on Haswell (2013) or newer CPUs. Checked: `scripts/build-v3.sh rustc -p hsearch-types --lib -- --print cfg` lists `avx2`, `fma` and `bmi2`; the default build lists none of them.
- A global `.cargo/config.toml` setting was tried first and dropped on review: with no run-time dispatch a v3 binary dies on an older x86 CPU or on a VM that hides AVX2, so the fast build is opt-in.
- `hsearch_engine::check_cpu()` (upstream's `lucene_util::cpu::check_baseline`) runs first in `hsearch-node`. It probes every feature the build was compiled with, so it asks for the whole v3 table in a v3 build, nothing in a default build, and refuses to start naming the missing features instead of dying on an illegal instruction. It is best effort (the check is itself built for v3), only the `hsearch-node` binary calls it, and `--help` also fails first on an old CPU. The refusal path itself is tested upstream, not here.
- Test: `the_test_machine_has_the_features_the_build_requires` (passes in a default and in a v3 build).

## Measured: baseline x86-64 against x86-64-v3 (queries like the data)

| Variant | Build base / v3 | Query p50 base / v3 | recall@10 |
| --- | --- | --- | --- |
| HNSW plain, beam 200 | 5.5 s / 3.9 s | 0.87 / 0.81 ms | 1.0000 both |
| HNSW bbq, beam 200 | 5.6 s / 3.9 s | 0.96 / 0.96 ms | 0.9875 both |
| IVF bbq, 16 probes | 1.29 s / 0.99 s | 1.77 / 1.16 ms | 1.0000 both |
| IVF osq4, 16 probes | 1.43 s / 1.09 s | 2.47, 2.58, 2.80 / 1.24, 1.51, 1.55 ms (three runs each) | 1.0000 both |
| disk HNSW osq4, beam 400 | 5.5 s / 4.1 s | 1.32 / 1.12 ms | 1.0000 both |

Builds are 25 to 30% faster everywhere. IVF queries are 1.5x to 2x faster (the cluster scans and rerank are distance loops). HNSW plain and bbq queries are within noise: their per-node work is small and cache-bound at this size. Recall is identical in every row. AVX-512 against AVX2 at run time (`LUCENE_RUST_SIMD=avx2` or `avx512`, v3 build): IVF bbq 1.46 against 1.47 ms, IVF osq4 1.43 against 1.63 ms, HNSW bbq 1.16 against 1.04 ms; no consistent difference, so the default stays.

## Trade-off and downsides

- Default build: portable, runs the scalar kernels (about 25% slower to build and up to 2x slower to scan IVF than v3). v3 build: needs Haswell (2013) or newer with AVX2, FMA, BMI1/2, LZCNT, MOVBE, F16C visible to the guest; the startup check says so on anything older. Which artifact to ship is a release decision; the evidence supports v3 wherever the fleet is known to have those features.
- A pipeline that sets `RUSTFLAGS` itself must add `-C target-cpu=x86-64-v3` (the script does) or silently gets the scalar kernels.
- Floating-point summation order differs between the scalar and SIMD kernels, so scores can differ in the last bits between builds; no test compares scores across builds.
- Recall is identical in every row measured.

## Not done here

- ARM: NEON kernels are chosen by `cfg(target_arch = "aarch64")` upstream and need no flag; there is no run on an ARM machine. The ARM CI workflow is manual only (project decision), so this stays unrun until someone dispatches it.
- Per-kernel microbenchmarks and a run on AVX-512-only paths (popcount, VNNI) beyond the level comparison above.
- Run-time dispatch: lucene-rust selects the AVX2/FMA kernels with `cfg(target_feature)`, so one binary cannot choose at start-up. One portable binary that is also fast needs multiversioned kernels (each compiled for the baseline and for v3, picked once by CPU detection) as an additive upstream PR to huyz0/lucene-rust; it is not cheap and is not done. It would remove the choice between the two artifacts.
