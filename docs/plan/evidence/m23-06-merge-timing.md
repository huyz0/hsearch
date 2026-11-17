# M23.06 evidence: merge time and memory at 100k to 1M vectors

Provisional: every number below is from one shared 4 vCPU x86 cloud VM (16 GiB), release build compiled with `RUSTFLAGS="-C target-cpu=x86-64-v3"` (the AVX2/FMA kernels; a default x86-64 build runs the scalar ones and is slower, see M23.05), in-memory directory, one thread, other jobs possibly running. Single runs. They are relative, not absolute, and there is no ARM run. The corpus is the synthetic clustered one of M23.01 (64 Gaussian centres, 128 d, L2); queries are drawn like the data. SIFT1M and GIST1M were not reachable, so real-dataset behaviour is **unrun**.

`cargo run --release -p hsearch-engine --example vector_merge_bench -- [variant] [PER_SEGMENT] [SEGMENTS] [DIM]` (`HS_BEAM` sets the candidates, `HS_MERGE=0` skips the merge). The writer flushes every 10,000 buffered documents, so `4 x 25,000` leaves 12 flushed segments, which one forced merge turns into one. IVF clusters at 512 vectors and is queried at the calibrated depth (M23.04); HNSW and disk HNSW at 200 candidates.

## Merge of the flushed segments into one

| Variant | Vectors (flushed segments) | Merge | Merge rate | Peak RSS above its pre-merge level | Index before / after | Merged query p50 | recall@10 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| IVF bbq | 100,000 (12) | 1.2 s | 85k/s | 250 MiB | 103.2 / 103.3 MiB | 0.52 ms | 1.0000 |
| IVF osq4 | 100,000 (12) | 1.1 s | 91k/s | 274 MiB | 107.7 / 107.8 MiB | 0.40 ms | 1.0000 |
| HNSW plain | 100,000 (12) | 8.8 s | 11k/s | 178 MiB | 52.9 / 54.3 MiB | 0.53 ms | 1.0000 |
| HNSW bbq | 100,000 (12) | 9.4 s | 11k/s | 165 MiB | 55.9 / 57.4 MiB | 0.47 ms | **0.4970** |
| disk HNSW osq4 | 100,000 (12) | 10.2 s | 9.8k/s | 293 MiB | 126.3 / 127.8 MiB | 0.49 ms | 0.9700 |
| IVF bbq | 400,000 (40) | 7.4 s | 54k/s | 950 MiB | 412.7 / 413.4 MiB | 1.55 ms | 1.0000 |
| IVF osq4 | 400,000 (40) | 6.8 s | 59k/s | 1,077 MiB | 430.7 / 431.1 MiB | 0.74 ms | 1.0000 |
| HNSW plain | 400,000 (40) | 66.1 s | 6.1k/s | 661 MiB | 211.7 / 217.4 MiB | 0.93 ms | 0.9580 |
| HNSW bbq | 400,000 (40) | 65.5 s | 6.1k/s | 672 MiB | 224.0 / 229.7 MiB | 0.88 ms | **0.2610** |
| disk HNSW osq4 | 400,000 (40) | 68.5 s | 5.8k/s | 1,267 MiB | 505.4 / 511.3 MiB | 1.08 ms | 0.9200 |
| IVF bbq | 1,000,000 (100) | 28.3 s | 35k/s | 2,468 MiB | 1,031.7 / 1,033.7 MiB | 3.42 ms | 1.0000 |

Write rates for the same runs: IVF 43k to 57k vectors/s, HNSW variants 12k to 14k vectors/s.

## Merged against unmerged, same data (HNSW variants, 200 candidates, `HS_MERGE=0` skips the merge)

| Variant | Vectors | Unmerged (12 or 40 segments): p50, recall@10 | Merged (1 segment): p50, recall@10 |
| --- | --- | --- | --- |
| HNSW plain | 100,000 | 1.32 ms, 1.0000 | 0.58 ms, 1.0000 |
| HNSW bbq | 100,000 | 1.63 ms, 0.9400 | 0.57 ms, 0.4970 |
| disk HNSW osq4 | 100,000 | 1.15 ms, 0.9990 | 0.45 ms, 0.9700 |
| HNSW plain | 400,000 | 3.62 ms, 1.0000 | 0.93 ms, 0.9580 |
| HNSW bbq | 400,000 | 3.84 ms, 0.7540 | 0.90 ms, 0.2610 |

## What this shows

- **IVF merges faster than HNSW at every size**: 85k, 54k and 35k vectors/s at 100k, 400k and 1M against 11k, 6.1k vectors/s for HNSW at 100k and 400k (7.7x to 10x at the same size, 400k disk HNSW against IVF osq4). IVF's rate falls as the segment grows (the segment count also grew, 12, 40, 100; the HNSW rate falls too, so the cause was not isolated). A 1M-vector IVF merge took 28 s.
- **Peak memory** above the pre-merge level was 2.3x to 2.5x the index size for IVF and 2.3x to 3.4x for HNSW and disk HNSW. The column is the process high-water mark minus the RSS after the writes, so it is an upper bound on the merge's own peak (a write-phase transient above that level would be counted). A 1M-vector IVF merge peaked at 2.5 GiB above, so a merge budget must reserve more than the index size.
- **Merging does not change the index size** (within 3%), and it cuts query latency 2.5x to 4x here: 12 to 40 small graphs are each walked.
- **Recall at a fixed beam falls when the graphs are merged**, for every HNSW variant and most for 1-bit codes. Unmerged, a search walks every segment with 200 candidates each (2,400 to 8,000 candidates in all); merged, the one large graph gets 200. That is the main difference between the two columns, not a defect shown here, but it means the `ef_search` that held recall before a merge does not hold after it. Merged bbq needed 800 candidates for 0.908 and 1,600 for 1.0000 at 100,000 vectors (plain needed 200). Why 1-bit codes lose most was not isolated; ranking near ties inside a cluster of about 1,500 points on this corpus is a guess, untested. osq4 disk HNSW lost the least (0.999 to 0.970 at 100k, 0.92 at 400k). A bbq HNSW field should be queried with a larger `ef_search` after merges, or use osq4 or an IVF field; IVF recall was 1.0 merged and unmerged.

## Observation for the engine owner

`Engine::merge` merged nothing when called straight after `refresh` on flushed but uncommitted segments (12 segments before and after, 0.0 s); after `commit` it merged them. The bench commits before merging. The doc comment says only "explicit bounded maintenance"; whether a merge should flush or refuse is a decision for the engine adapter, so nothing was changed here.

## Not done here

- Real-dataset merge timing (SIFT1M, GIST1M), ARM, and a merge of 1M vectors for HNSW (66 s at 400,000 extrapolates to about 3 minutes).
- Merging with deletions and the `merge_clustering` option that reuses the largest source's clusters, which upstream offers for IVF and could cut the superlinear cost; not measured.
