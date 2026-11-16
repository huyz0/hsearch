# M04.02 remote range input

`provisional`: one 4-vCPU Linux sandbox, RustFS 1.0.4 (the release binary, not the digest-pinned image: no Docker daemon here) on loopback, no provider network, no ARM, one to three runs per cell. Numbers are relative, not absolute.

## What changed

- **lucene-rust** (huyz0/lucene-rust#431, merged `2ec6edc8`, additive, no format, FFI or default change): `PageCacheConfig::heap_budget` bounds the bytes of files held whole on the heap (pinned files, one-page files, and without demand paging the files below `spill_min_bytes`); a file past it fails to open before a byte is fetched. `PageCacheStats` gains `heap_open_bytes`, `heap_opens`, `demand_paged_opens`. `lucene_store::demand_paging_available()` says whether `userfaultfd` works in the process. Default unbounded: whole files are read exactly as before, uncounted. A bounded budget serves them through one indirect call per access (not benchmarked).
- **Pin** bumped `17a6254c` to `2ec6edc8` (the bridge contract and old-index corpus pass at the new pin: `cargo test -p hsearch-engine`).
- **`RangeCacheSettings::heap_budget`** (default 1 GiB, refused at 0 or unbounded) and **`RangeCacheMetrics`** (`demand_paging_available`, `demand_paging_configured`, cache stats), for a node to export. `RemoteRangeInput` is now `Supported` in `factory.rs`.
- **Defect found and fixed**: `EngineDirectory`'s inner directory forwarded neither `prefetch_files`, `prefetch_files_decoded`, `open_decoded`, `open_aligned` nor `read_at`, so a range-reading backend fetched one file per round trip. The new depth test went from 17 sequential round trips to 3 at 17 files.

## Conditions from the M02.06 review

| Condition | Result |
|---|---|
| C1 cold-open and first-query depth and count | Test `a_cold_open_and_first_query_take_few_sequential_round_trips` (17 files, 3 ms per read): open 3 round trips (17 requests), the all-query set 0 more; gate: open at most 4, queries at most 3. Seen red at 17 before the forwarding fix. RustFS, `cache_bench`, default settings, waves of requests started with none in flight (a lower bound): 20k docs (33 MiB, 33 files) open 2 round trips, 39 GET, then 0 GET for the first range query; 200k docs (333 MiB) open 6 round trips, 371 GET, first query 0 GET. Open GET bytes vary 28 to 140 MiB between runs with the live tuner (the window follows timings); `store_profile` fixes it. |
| C2 `userfaultfd` | Detected and exported (`RangeCacheMetrics`). Here it works, so the demand path ran. With it off (`demand_paging: false`, the likely production case): 200k docs, 333 MiB corpus, 393 GET, 333 MiB fetched once (each byte once), open 686 ms against 370 ms demand-paged, 60 spill opens, heap held 290 KB by 261 whole files; with spilling also off the 33 files are held whole on the heap (34.9 MiB at 20k docs), which is what `heap_budget` bounds: test `without_demand_paging_heap_residency_stays_within_its_budget` (budget respected; a budget of 1 byte fails the open). Small-file heap is bounded per cache, so across open readers sharing a cache. |
| C3 copy and blocking of `ObjectBlobStore::get_range` | Kept, no upstream method needed. 8 MiB object on RustFS, `s3_range_bench`: `get_range` against reading the backend directly, 256 KiB, 1 MiB, 4 MiB at 1, 8, 32 threads: throughput within about 10% either way (e.g. 1 MiB x 8: 1818 against 1736 MiB/s; 4 MiB x 1: 911 against 1028 MiB/s, the largest gap), p50 within noise. It blocks one thread per in-flight read, bounded by the cache's `concurrency` and in-flight limits. |
| C4 gate | Request counts and sequential depth at the above scales, not a share-of-files bound. |

## Remote corpus with errors

`ranged_snapshot` (17 tests): answers equal a local directory with no LIST and no HEAD; an error injected at each of the 29 range requests, of kinds unavailable, deadline and throttled, with a disk tier a quarter of the corpus, gives an error or the exact answer, and a fresh open recovers; each byte is requested at most once; the snapshot over a shared cache opens with no request. `remote_rustfs` passes against the real store.

## Limits

- Wave counting is a lower bound; the in-memory depth test sleeps 3 ms per read, it does not model a real network.
- **Default change:** upstream's `heap_budget` is unbounded; hsearch defaults to 1 GiB, so a snapshot whose whole-held files exceed it now fails with `Error::Limit` where it used to open. It is a per-cache setting (raise it per index); not measured against production sizes. `RangeCacheMetrics` is a plain value, not yet wired into `hsearch-observe`; the per-reader heap bound is not shown, only the cache-wide one. The RustFS numbers come from `cache_bench`/`s3_range_bench` runs, not from tests.
- `heap_budget` default and its refusal of a pinned file past it are provisional; no production sizing exists.
- A bounded budget's indirect call per access is not benchmarked; the default is unchanged.
- The M02.06 gate scale of 1M documents was not run; 200k documents (333 MiB) was the largest.
- Open request count grows with files per segment (371 GET for 321 files at 200k docs).
