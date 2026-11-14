# M02.12 evidence: lucene-rust vector audit

Provisional: every number below was measured on one shared cloud VM (Intel Xeon 2.1 GHz, 4 vCPU, AVX-512 present, other jobs running at times), release builds, rustc 1.97.1. They are relative, not absolute. No ARM machine was available.

## Revisions

| Name | Revision | Role |
| --- | --- | --- |
| pin | `46e280bc2a208544fb36ecd95932e89cd8bc5618` (31 Oct 2026) | what hsearch's `Cargo.toml` builds today |
| main | `918cee0283ff6e41338fafca7146ef905cec788e` (14 Nov 2026, 204 commits after the pin) | where the 13 Nov table was inspected |

**The central finding:** the vector table describes `main`, not the pin. At the pin there is no `lucene-ivf`, `lucene-disk-hnsw` or `lucene-vector-quant` crate and no `lucene-util/src/cpu.rs`. hsearch cannot use IVF, disk HNSW or BBQ/OSQ until it bumps its pin (PR-53 says a bump follows each upstream change). M23.01 stays on the pin; M23.02 and M23.03 need a new pin first.

## Commands run

- Pin: `cargo test --release -p lucene-codecs {hnsw,vectors,knn,quantiz}`: 100, 206, 14, 30 passed, 0 failed. `-p lucene-search {knn,hnsw,vector}`: 15, 1, 59 passed. `-p lucene-index {hnsw,vector}`: 4, 90 passed. `cargo bench -p lucene-search --bench knn_multi_segment`.
- Main: `cargo test --release -p lucene-ivf -p lucene-disk-hnsw -p lucene-vector-quant`: 91 (41+3+11+3+2+26+5), 26 (13+13), 53 passed; 1 ignored (`rabitq::tests::microbench`); 0 failed. `cargo bench -p lucene-util --bench vector_kernels`, `-p lucene-ivf --bench ivf_path` (partial: stopped after the scorer group).
- `cargo test --release -p lucene-util` fails 2 tests (`from_words_rejects_ghost_bits_in_debug`, `int_to_byte4_rejects_values_outside_javas_domain_in_debug`); both are `should_panic` tests that need debug assertions, and pass in a debug build (347 passed, 0 failed). Not a defect.

## Table rows

| Row | Verdict | Evidence |
| --- | --- | --- |
| HNSW | **Confirmed at the pin and main.** Concurrent build and merge at hsearch segment sizes: **unrun**. | Tests above pass. `hnsw_concurrent.rs` (1,105 lines) exists at the pin and its tests are in the 100 passing. Bench `knn_multi_segment`: 4,000-doc single segment 174 µs; four segments sequential 1.31 ms, concurrent 2.69 ms (concurrent search is slower than sequential at this size, provisional); k=100 3.72 ms vs 3.86 ms. No bench builds or merges at hsearch segment sizes. |
| HNSW + quantization | **Confirmed**, but split: Lucene scalar quantization is at the pin (`scalar_quantized_vectors.rs`, 30 `quantiz` tests); BBQ/OSQ/RaBitQ quantizers are `main` only. | `lucene-vector-quant`: 53 tests pass. `ivf_path` scorer group at 100 d, one block: BBQ 617 ns, OSQ2 677 ns, OSQ4 3.24 µs, SQ7 3.18 µs, RaBitQ 602 ns, ERQ3 1.12 µs, ERQ4 1.41 µs. |
| HNSW disk-based | **Confirmed on main only.** Range reads from the object store: the `RangeSource` trait and `prefetch` exist, so the engine side is ready; the hsearch implementation is **unrun**. | `lucene-disk-hnsw`: 26 tests pass, recall asserted at >= 0.9 per vector encoding (`tests/disk_hnsw.rs:227`). `examples/disk_hnsw_bench` needs a SIFT/GIST-style dataset that is not in this environment: **unrun**. |
| IVF | **Confirmed on main only.** Update and delete: IVF is write-once per segment, as the table says. Deletes are applied at search through the `accept` filter and dropped at merge (`merge.rs` takes only live docs). No in-place update exists, by design. | `lucene-ivf`: 91 tests pass, recall asserted >= 0.9 (`tests/ivf.rs:154,188`), merge recall and reuse-versus-recluster tests included. |
| IVF + quantization | **Confirmed on main only.** One `Quantizer` trait shared with HNSW and disk HNSW. | `auto_quantizer.rs` and the 53 quantizer tests pass. |
| IVF disk-based | **Refuted: the "not confirmed" cell was wrong.** The `.ivm/.ivp/.ivv` layout is built for range reads: one contiguous range per probed cluster, one small range per reranked candidate, `.ivm` kept in memory, codes at 1 bit per dimension plus 16 B (BBQ). A separate disk layout is not needed. The remaining gap is on hsearch's side: a `RangeSource` over the object store with a page cache. | `crates/lucene-ivf/src/lib.rs` module docs and `reader.rs` `RangeSource`. Request counts per query were not measured here: **unrun**. |
| Sparse and multi-vector | Present on main (`lucene-sparse`, `lucene-tensor`); not audited, out of scope for PR-22. | Not run. |
| SIMD | **Partly refuted.** Compile-time AVX2 and NEON kernels: confirmed (`target_feature` gates in `simd.rs`; `.cargo/config.toml` sets x86-64-v3). AVX-512: run-time detected but only for specific kernels (fast-scan `lut16_accumulate32_wide`, bitset expansion, compaction, MaxSim), through `cpu.rs` with a `LUCENE_RUST_SIMD` cap; float dot, L2 and cosine are AVX2/FMA, not AVX-512. **No startup check of the CPU floor existed**; closed by lucene-rust#307 (merged as `02dcad21`). SVE: nothing exists (grep finds none); whether it pays on Graviton4 is **unrun**, needs the hardware. NEON: compile-checked locally; the PR's CI `gate (arm64)` and `lucene-search in release (arm64)` jobs passed, so the NEON tests ran on ARM, but no NEON benchmark was run. | `vector_kernels` bench, SIMD against scalar: dot 768 d 31.7 ns vs 198 ns (6.3x); L2 768 d 37.7 vs 160 ns (4.2x); cosine 768 d 66 vs 579 ns (8.8x); nibble LUT 768 d 165 ns vs 5.83 µs (35x); quantize residual 768 d 304 ns vs 3.68 µs (12x); gather of 32 random 100-d rows 1.93 µs with prefetch vs 3.41 µs without (768 d: 10.1 vs 10.9 µs). |
| Encryption | Not in scope of this task (row not audited). | Not run. |

## Quantizer settings OpenSearch mappings can expose

`opensearch-plugin/.../KnnFieldConfig.java` on main already maps `knn_vector`:

- `engine: lucene` (default): `method.name: hnsw`, `parameters.m` (max `MAX_M`), `ef_construction` (default 100, max 3200), `encoder` (Lucene scalar), `mode: in_memory | on_disk`, `compression_level`. `ef_search` is refused (set it in the query). `faiss` is refused. `lucene`-engine `ivf` is refused (it needs a trained model).
- `engine: extended` (this repo's own, opt-in): `method.name: ivf` (cluster size 16 to 65,536; quantizers `bbq` default, `osq2/4/7`, `erq2` to `erq8`, `rabitq`, `auto`; rerank encoding `f32/f16/bf16/int8`), `hnsw` over quantized codes (`rabitq`, `erq2` to `erq8`, default `erq2`), and `disk_hnsw` (default `bbq`).

hsearch decision for M23: expose `method.name`, `m`, `ef_construction`, `space_type`, `mode`, `compression_level` for HNSW and the `extended` engine's `ivf` and `disk_hnsw` parameters; drop `faiss`, `ef_search` in the mapping (already refused upstream) and the trained-model path.

## Recall@10 floors per variant

Corpora that exist today are synthetic test corpora inside lucene-rust's tests; no standard-corpus run was possible here. Frozen floors, to be re-measured on a stated corpus (SIFT1M or GIST1M, 128 and 960 d) before M23 acceptance:

| Variant | Floor | Source of the number |
| --- | --- | --- |
| HNSW float | 0.95 at ef 100 | proposed; upstream tests assert only coarse floors |
| HNSW + quantized | 0.90 after exact rerank | proposed |
| Disk HNSW | 0.90 | upstream test floor, `disk_hnsw.rs:227` |
| IVF + quantized | 0.90 at calibrated depth | upstream test floor, `ivf.rs:154,188` |

Only the disk-HNSW and IVF floors are measured upstream; the HNSW rows are proposals and are marked unrun.

## Gaps, upstream changes and waiting tasks

| Gap | Additive upstream change | Waiting task |
| --- | --- | --- |
| Startup CPU-floor check missing (confirmed) | [huyz0/lucene-rust#307](https://github.com/huyz0/lucene-rust/pull/307), merged `02dcad21`, all 18 CI checks green incl. arm64: `cpu::{missing_baseline, check_baseline}`, additive, tested; nothing calls it yet, the node wires it in | node startup in M20 follow-ups; M23.01 |
| Pin lacks IVF, disk HNSW, BBQ/OSQ (confirmed) | none to write: bump the pin after checking the bridge contract and old-index corpus (PR-53) | M23.02, M23.03 |
| Concurrent HNSW build and merge at hsearch segment sizes (unrun) | measure first; a bench at 100k to 1M vectors per segment | M23.01 |
| Concurrent segment search slower than sequential at 4,000 docs (provisional) | measure at hsearch segment sizes before any change | M23.01 |
| Object-store `RangeSource` with page cache and batched ranges | hsearch side (M04.02); no engine change needed | M23.03 |
| SVE on Graviton4, NEON speedups (benchmarks unrun) | needs ARM hardware; manual ARM CI only, per project rule | M23 follow-up |

Not done, so as to be clear: no upstream benchmark run reached a standard corpus; the `disk_hnsw_bench`, `knn_formats` and `hnsw_quant_bench` examples all need a dataset that is not here.
