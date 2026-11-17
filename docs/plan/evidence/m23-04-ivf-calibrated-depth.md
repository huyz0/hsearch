# M23.04 evidence: the calibrated IVF probe depth

Provisional: every number below is from one shared 4 vCPU x86 cloud VM, release build, in-memory directory, one thread, other jobs possibly running. They are relative, not absolute, and there is no ARM run. The corpus is the synthetic clustered one of M23.01 (64 Gaussian centres, unit-variance noise, 50,000 x 128 d, L2, 200 queries). It is easy: well separated clusters saturate recall at 1.0, so these numbers show where the depth fails, not how close to the floor it sits. SIFT1M and GIST1M were not reachable, so the real-dataset rerun is **unrun**.

## Finding

M23.02 concluded that upstream's automatic probe depth was too shallow (0.325 recall at 50,000 vectors), required `nprobes` in the mapping, and listed an adaptive depth as not built. That was a bench artefact. Upstream already calibrates: `IvfParams::default()` records, when a segment is written, the share of the field's clusters to probe for 0.95 recall@10 on a sample of the field's own vectors (`lucene-ivf` `calibrate`, `Probes::Auto`), and a search with `nprobes` 0 uses it. M23.02's queries were drawn around other centres than the data (the bench seeded the centres by the query seed), so they lay far from every cluster, where neighbours spread over many clusters and a depth calibrated on in-distribution samples is too shallow.

## Measured (50,000 x 128 d, IVF bbq, clusters of 512, 100 candidates reranked)

`cargo run --release -p hsearch-engine --example vector_bench -- ivf-bbq 50000 128 100 [NPROBE]`; `HS_QUERIES=near` draws the queries around the data's centres.

| Queries | nprobes | p50 / p99 | recall@10 |
| --- | --- | --- | --- |
| like the data | automatic (0) | 0.56 / 0.91 ms | 1.0000 |
| like the data | 4 | 0.97 / 1.44 ms | 1.0000 |
| like the data | 16 | 1.61 / 2.47 ms | 1.0000 |
| far from every cluster | automatic (0) | 0.62 / 0.98 ms | 0.2745 (0.325 in the M23.02 run; run to run differences from separate builds) |
| far from every cluster | 4 | 1.45 / 2.13 ms | 0.5965 |
| far from every cluster | 16 | 2.03 / 3.27 ms | 0.9805 |

On queries like the data the automatic depth is the cheapest setting and already at the ceiling; a fixed 16 probes costs 2.9x the latency for nothing. Against HNSW on the same queries (build, index, p50, recall): HNSW plain beam 100 5.3 s, 26.5 MiB, 0.72 ms, 1.0000; IVF bbq automatic 1.2 s, 51.6 MiB, 0.56 ms, 1.0000; IVF osq4 automatic 1.1 s, 53.8 MiB, 0.54 ms, 1.0000; IVF bbq f16 automatic 1.2 s, 39.4 MiB, 0.75 ms, 1.0000. All saturate, so equal recall is not separated by this corpus.

## What changed

- `nprobes` is optional in an IVF mapping. Without it the field's depth is the one each segment calibrated (the stored line carries `np=0`, which already read back). `nprobes` still sets a field default and `method_parameters.nprobes` a per-query depth. The mapping refusal for a missing `nprobes` is removed and its test replaced by `an_ivf_field_without_nprobes_searches_at_the_depth_its_segment_calibrated` (seen red: it was refused).
- `the_calibrated_depth_holds_recall_at_hundreds_of_clusters_for_queries_like_the_data` (30,000 vectors, about 470 clusters, queries like the data, after a merge): automatic depth 1.0000, one probe 0.3175; asserts at least 0.95 and above one probe.
- The node test only shows a mapping without `nprobes` is accepted and searches (sixteen documents return the same hits at any depth); the depth itself is held by the engine test above and the bench.
- The `Query::Knn.nprobe` doc and the M23.02 evidence are corrected. The bench gets `HS_QUERIES=near`.

## Downsides and limits

- The calibration targets queries like the field's own vectors. A query far from every cluster (out of domain, or a wrong embedding model) gets a shallow probe and low recall at the automatic depth (0.27 here); a deeper `nprobes` is the remedy (0.98 at 16). The depth is chosen by the segment, so it is not a recall guarantee for such queries.
- Calibration samples 64 of the field's vectors when a segment is written, a small fraction of the clustering cost; its write cost was not measured separately.
- The calibrated depth is fixed per segment at write time; there is no per-query adaptive depth that widens when the nearest clusters are far (a query-time rule on the distance to the nearest centroid would need upstream support and is not built).
- Real-dataset recall, ARM and merge timing at 100k to 1M vectors per segment are unrun.
