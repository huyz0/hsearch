# M02.03 evidence: range-native remote inputs

## Finding: lucene-rust already has it, so hsearch reuses it

At the pin in use (`8c010db7`) lucene-rust ships the whole read path for an index on an object store, in
`crates/lucene-store/src/`: `page_cache.rs` (`PageCache`: fixed pages, adjacent missing pages merged into one ranged
request up to `max_request`, one request per page across concurrent readers, an optional bounded disk tier with LRU
eviction and pinned files, `stats()`), `cached_blob_directory.rs` (`CachedBlobDirectory`: a `Directory` over any
`BlobStore`, opening a file from its length alone), `paged_input.rs` (demand-filled `Input::External` on Linux
`userfaultfd`, `ExternalBytes::will_need`) and `blob_directory.rs` (the `BlobStore` trait with `get_range`,
`list_with_sizes`, `size`). An earlier version of this PR built a page cache in `hsearch-engine`; it duplicated that and
was deleted. **No upstream lucene-rust change is needed for range reads.**

## What this PR adds (`crates/engine/src/range.rs`)

- `ObjectBlobStore<B: ObjectBackend>`: lucene-rust's `BlobStore` over `hsearch-object`'s `ObjectBackend`, as small as the
  two APIs allow. `list`, `list_with_sizes` and `size` come from the published manifest (`ManifestEntry`: key, version
  token, length), so reading costs no LIST and no HEAD, and every `get_range` is one `read_range` pinned to the
  manifest's version under a fresh bounded `RequestContext`. Read-only: `put`/`delete` refuse. Request counts, bytes and
  cost attribution stay in the backend's own ledger (`S3Store` already attributes them); the cache adds `PageCache::stats`.
- `Snapshot::open_blob(store, cache, namespace, schema)` and `open_blob_with(store, config, schema)`: a read-only snapshot
  through `CachedBlobDirectory`. Three lines; no cache, trace or wrapper type of hsearch's own.
- The large-object `put` (streamed or multipart) is a separate upstream task and is added to this store after this merges.

## Validation (`tests/ranged_snapshot.rs`, backend = counting/failing test double over `MemoryStore`)

- Answers (count and sorted hit ids for All, term and range, with a doc-values sort and a stored-field projection) equal
  the local directory's; zero HEAD calls; no LIST exists on the backend trait at all.
- A second open through a shared `PageCache` sends **0** requests (served from the disk tier).
- Working set 198,595 B against a 24,824 B disk budget, three full query rounds: answers exact, `disk_footprint_bytes`
  (24,576) never above the budget; the over-budget pages are re-fetched (23 requests, 315,451 B).
- A range error injected at each of the 17 range requests of an open plus query round: every outcome is an error or an
  exact answer, never a wrong one, and a fresh attempt after the fault succeeds exactly.
- A manifest naming a wrong version, a missing file, or a deadline over five minutes is an error.
- Per-query trace (2,000 batches, 387,159 B in 33 files, 4 KiB pages): no page is fetched twice, the cache's
  `fetched_bytes` equals the request trace, and a repeated query sends nothing. Measured request bytes by phase (runs
  differ, see below): open 223-387 KB, then a term query 2 requests/32 KiB, a doc-values sort 1 request/16 KiB, range
  and stored-field queries 0.

## Findings about upstream behaviour (reported, not fixed here)

1. **Laziness depends on `userfaultfd` and varies run to run.** `CachedBlobDirectory` reads a file whole into
   `Input::Owned` when the process-wide pager is unavailable, when a file is at most two pages, or when `page_size` is not a
   multiple of the OS page (4 KiB): with 256 B to 1 KiB pages every file was fetched whole at open. With 4 KiB pages the
   same fixture fetched between 223 KB and the full 387 KB at open across repeated runs. The DoD item "no full-file heap
   allocation" therefore holds only where the pager runs; elsewhere the fallback is a full-file `Vec`. A bounded fallback
   (a disk-tier mapping instead of a heap copy) is an upstream follow-up, additive to `Input`, and is not done here.
2. The tests assert only invariants that hold in every environment; laziness is printed, not asserted.

## Benchmark and trade-offs (provisional: 4 vCPU container, release build, in-memory backend so only the wrapper's cost shows)

`cargo run --release -p hsearch-engine --example range_bench`, 8 MiB object, random offsets:

| request | via `BlobStore` | backend alone | wrapper adds |
|---|---|---|---|
| 4 KiB | 522 ns | 415 ns | 107 ns |
| 64 KiB | 4.9 us | 3.3 us | 1.6 us |
| 256 KiB | 19.0 us | 12.4 us | 6.6 us |
| 1 MiB | 123 us | 62 us | 61 us |

The addition is a fixed ~100 ns (context, poll, manifest lookup) plus one copy of the payload into the `Vec` the
`BlobStore` trait returns (`Payload` has no way to hand over its buffer; `hsearch-object` is outside this task). Against a
real object-store GET (milliseconds) it is under 0.1%. Not profiled with a sampling profiler.

- Memory: nothing here allocates beyond one `Vec` per request, at most `PageCacheConfig::max_request` (and 16 MiB by the
  store's own cap), times the cache's request concurrency. Bounds on resident memory and disk are the cache's
  (`disk_budget`, the pager's budget). Whether a request larger than `max_request` can arise was not tested.
- Each `get_range` blocks its calling thread on the backend future with a std-only `block_on`; `CachedBlobDirectory` already
  prefetches files on its own scoped threads, so no async runtime is entered. A cooperative runtime would need the
  upstream `BlobStore` to be async.
