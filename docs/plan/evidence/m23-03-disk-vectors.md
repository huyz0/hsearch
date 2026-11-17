# M23.03 evidence: disk HNSW and IVF read by range from the object store

Provisional: every number below is from one shared 4 vCPU x86 cloud VM, release build, one thread, other jobs possibly running. The object store is a file-backed counting backend with 1 ms added per range read, not S3 or RustFS, so request counts and bytes are exact for this layout but latencies are not. There is no ARM run. The corpus is synthetic and clustered (as M23.01 and M23.02); SIFT1M and GIST1M were not reachable, so the real-dataset rerun is **unrun**.

## What exists

- `Index::HnswDisk { quantizer }`, built with `VectorSpec::hnsw_disk(..)`, routes to upstream's `KnnVectorsFormat::HnswDisk` (`lucene-disk-hnsw`, same pin `df02d694`). The graph keeps `m` and `ef_construction`. Codes (any quantizer except `auto`) stay in memory in `.dhc` and score neighbours; the graph is stored as page-aligned node records in `.dhn`, one aligned read per expanded node. `.dhm` is the metadata, read whole.
- The mapping is `mode: on_disk` on a `knn_vector` with `method.name: hnsw` (or none), plus `quantizer` (default `bbq`). `mode: in_memory` is the default and changes nothing. `auto`, the HNSW `scalarN` names and `none` are refused for a disk field with that reason, and `mode: on_disk` with `method.name: ivf` is refused because an IVF field is already read by range. The stored line is `knn_vector:dim=..,space=..,disk,m=..,efc=..,q=..`; HNSW and IVF lines and mappings without a disk field keep their bytes and hash. A disk field takes `ef_search` (the beam) and refuses `nprobes` by name, as an HNSW field does.
- IVF needed no new layout: `.ivm` is read whole, `.ivp` is one range per probed cluster and `.ivv` is one small range per reranked candidate, so it already reads by range through the M04.02 input.
- `RangeCacheSettings.warm_on_open` (default **false**) is new. Upstream's `SegmentReader::open` advises the vector files random and warms them, which reads every vector file whole at open. With the setting off, open reads headers and metadata only. It is a setting under project rule 8 because it has a measured downside (below).

## Tests

- `crates/engine/tests/vector.rs`: `every_disk_hnsw_variant_meets_its_recall_floor` (floor 0.90 for every quantizer), `a_filtered_disk_hnsw_search_returns_k_and_only_matches`, `a_disk_hnsw_checkpoint_reopens_with_the_same_top_k`, `deleted_and_replaced_vectors_leave_disk_hnsw_results_before_and_after_a_merge`. The delete, replace and merge helper is shared with IVF and takes the spec; it ignored its spec argument at first, so the disk test silently ran IVF, which the compiler's unused-variable warning showed and a fix corrected.
- `crates/node`: unit tests for the `on_disk` parse, every refusal and the spelling round trip (red when `on_disk` is ignored), and `an_on_disk_hnsw_field_is_mapped_searched_and_survives_a_restart`.
- `crates/engine/tests/vector_ranged.rs`: 20,000 x 128 d vectors published to the counting backend, opened with `Snapshot::open_blob_settings` through the demand-paged page cache with an 8 MiB cache against a 10 to 24 MiB index. Cold, the same queries again (warm), and 50 other queries (steady). Asserts recall at least 0.90 in all three, the cache smaller than the index, no request at all on the warm pass, at most one request a query on the steady pass, open bytes under a tenth of the index by default and over half with `warm_on_open`, and IVF cold requests and round trips under a third of HNSW's.

## Range reads per query (20,000 x 128 d, cache 8 MiB)

Cold is the average over 50 queries from an empty cache, so it includes the first queries' fill; "worst" is the first query.

| Variant | Index | Open | Cold per query: requests, KiB, round trips | Worst cold query | Warm | Steady (other queries) | recall@10 cold / steady |
| --- | --- | --- | --- | --- | --- | --- | --- |
| HNSW plain, beam 200 | 10.6 MiB | 51 requests, 0.1 MiB | 51.7, 215, 17.9 | 1,512 requests, 423 round trips | 0 | 0.0 requests | 0.9960 / 0.9980 |
| HNSW disk bbq, beam 400 | 24.3 MiB | 63 requests, 0.1 MiB | 72.2, 293, 38.1 | 950 requests, 485 round trips | 0 | 0.3 requests, 1.3 KiB | 0.9580 / 0.9600 |
| IVF bbq, nprobe 16 | 20.6 MiB | 57 requests, 0.1 MiB | 7.5, 205, 2.3 | 167 requests, 69 round trips | 0 | 0.0 requests, 1.5 KiB | 0.9940 / 0.9840 |
| IVF bbq, nprobe 16, `warm_on_open` | 20.6 MiB | 68 requests, **19.8 MiB** | 1.3, 17, 0.9 | 34 requests, 23 round trips | 0 | 0.0 requests | 0.9940 / 0.9840 |

The disk-HNSW layout is **not** a win on requests here. It issues more requests and round trips per cold query than stock HNSW (72 against 52 requests, 38 against 18 round trips) at a lower recall, even with the beam prefetch, because each expanded node is its own aligned read where the plain HNSW file keeps neighbouring vectors close and the page cache fetches them together. Its reason to exist is memory, not requests: only the codes (`.dhc`, 0.6 MiB here) must be resident, and a node record is read when it is expanded. The index is 2.3x the size of plain HNSW because the raw vectors are kept in the flat vector file and in the node records.

IVF is the layout that suits range reads: a cold query averages 7.5 requests in 2.3 round trips, and 16 probes read about 205 KiB of a 20.6 MiB index.

## Open cost: the `warm_on_open` finding

Upstream warms the vector files when a segment opens, so the first measurement of this task showed a 52 MiB read at open for a 52 MiB index, which defeats range serving. With `warm_on_open` off (the default) open reads 0.1 MiB (51 to 68 requests) and the first queries pay for their pages; with it on, open reads 19.8 MiB and queries are then served from the cache (1.3 requests, 17 KiB a query). The setting trades the open cost against first-query latency, which is a deployment choice (a long-lived reader that opens once may want it on; a reader that opens many cold segments does not), so it is a setting. The switch inside lucene-rust is process global (`set_warm_random`), and the engine applies it when a snapshot is opened through `open_blob_settings`; two snapshots opened with different values in one process will see the later one.

## In-memory cost, 50,000 x 128 d (bench)

`cargo run --release -p hsearch-engine --example vector_bench -- [variant] [N] [DIM] [BEAM] [NPROBE]`, in-memory directory.

| Variant | Build | Index | Peak RSS over baseline | Query p50 / p99 | recall@10 |
| --- | --- | --- | --- | --- | --- |
| HNSW plain, beam 200 | 5.2 s | 26.5 MiB (555 B/vector) | 83 MiB | 1.24 / 1.54 ms | 0.9545 |
| HNSW plain, beam 400 | 5.0 s | 26.5 MiB | 83 MiB | 1.48 / 2.74 ms | 0.9890 |
| disk bbq, beam 200 | 5.4 s | 60.8 MiB (1,274 B) | 141 MiB | 2.03 / 3.17 ms | 0.9150 |
| disk bbq, beam 400 | 5.5 s | 60.8 MiB | 141 MiB | 2.56 / 4.15 ms | 0.9485 |
| disk osq4, beam 400 | 5.3 s | 63.2 MiB (1,325 B) | 146 MiB | 1.56 / 2.39 ms | 0.9930 |

The code choice matters for disk HNSW: `osq4` reaches 0.993 at beam 400 where 1-bit `bbq` reaches 0.9485, and is faster. `bbq` stays the default because it has the smallest codes, but a disk field that must hold a recall target should take `osq4` or an `erq`. A beam of 400 is not enough for `bbq` on this corpus.

## Trade-offs and downsides

- Disk HNSW costs more requests and round trips than plain HNSW per cold query and takes about 2.3x the bytes. It is the right choice only when the in-memory footprint of a graph with full vectors is the constraint.
- The index is larger than HNSW in every disk variant (raw vectors are kept next to the codes).
- `warm_on_open` is process global in the dependency, as above.
- Cold figures are averages over 50 queries; the worst first query still reads hundreds of requests for HNSW variants (1,512 and 950), so a cold HNSW reader needs a prefetch or `warm_on_open` to bound the first query. IVF's worst cold query is 167 requests.

## Not done here

- Real-dataset recall (SIFT1M, GIST1M), ARM, SIMD kernels on ARM and x86 for the codes, and S3 or RustFS rather than the counting file backend: unrun.
- Merge timing at 100k to 1M vectors per segment, concurrent-segment search, an adaptive IVF probe depth: unrun or not built (as in M23.02).
- A cache sized against an index that is much larger than memory (here the index is 1.3x to 3x the cache).
