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

All page sizes are 4 KiB and read-ahead is fixed at one page (`download_window: Some(page)`); see finding 1.

- Answers (count and sorted hit ids for All, term and range, with a doc-values sort and a stored-field projection) equal
  the local directory's; zero HEAD calls; the backend trait has no LIST at all.
- **Range-native, asserted** wherever lucene-rust can demand-fill 4 KiB pages (`userfaultfd`, true here and on the
  Linux CI runner): open plus every query fetch under half of the files. Measured, identical in repeated runs, on
  387,159 B in 33 files: open 41 requests/67,671 B (17%), then a term query 8 requests/32,768 B, a range query
  2 requests/8,192 B, a doc-values sort 1 request/4,096 B, stored-field projection 0, a repeated query 0. The
  test asserts the 50% bound only when the pager is active; elsewhere files are read whole and the test says so.
- No byte is requested twice: per file the traced ranges are disjoint, and the cache's `fetched_bytes` equals the trace.
- A second open through a shared `PageCache` sends **0** requests (served from the disk tier).
- Working set 198,595 B against a 12,412 B disk budget, three full query rounds: answers exact, `disk_footprint_bytes`
  (12,288) never above the budget, and the pages read (59,331 B) exceed the budget, so the cache had to evict.
- A range error injected at each of the range requests of an open plus query round: every outcome is an error or an
  exact answer, never a wrong one, and a fresh attempt after the fault succeeds exactly.
- A manifest naming a wrong version, a missing file, or a deadline over five minutes is an error. A cache whose
  `max_request` or `page_size` exceeds the store's 16 MiB cap is refused at `open_blob`.
- Not tested: the real `S3Store` path (its truncation and short-body behaviour), two manifests under one namespace,
  memory bounds of pinned files (pinned extensions are read whole into memory and pinned on disk, so they may
  exceed `disk_budget`; this PR sets none), and a request larger than the cap.

## Findings about upstream behaviour (reported, not fixed here)

1. **Read-ahead is tuned from measured latency.** With `download_window: None` (the default) the cache picks a window
   between one page and `max_request` from the store's latency and throughput. Against a near-zero-latency backend
   it picked windows that fetched whole files: the same fixture fetched 190-387 KB at open across identical runs.
   Fixing the window to one page made it deterministic (67,671 B). Production should set the window deliberately
   against measured S3 latency (M04.02), because a large window trades GET count against bytes fetched.
2. **Without `userfaultfd`, or when `page_size` is not a multiple of the OS page, or for a file of at most two pages,
   files are read whole into `Input::Owned`**: with 256 B-1 KiB pages every file was fetched whole at open. The DoD item
   "no full-file heap allocation" holds only where the pager runs and the page size fits. A bounded fallback (a disk-tier
   mapping instead of a heap copy) is an upstream follow-up, additive to `Input`, and is not done here.
3. The cache keys a page by namespace, file name and length, not version, and a disk tier can outlive a snapshot, so
   `open_blob`'s namespace must be derived from the manifest's root version (documented on the function).

## Benchmark and trade-offs (provisional: 4 vCPU container, release build, in-memory backend so only the wrapper's cost shows)

`cargo run --release -p hsearch-engine --example range_bench`, 8 MiB object, random offsets:

| request | via `BlobStore` | backend alone | wrapper adds |
|---|---|---|---|
| 4 KiB | 522 ns | 415 ns | 107 ns |
| 64 KiB | 4.9 us | 3.3 us | 1.6 us |
| 256 KiB | 19.0 us | 12.4 us | 6.6 us |
| 1 MiB | 123 us | 62 us | 61 us |

The addition is a fixed ~100 ns (context, poll, manifest lookup) plus one copy of the payload into the `Vec` the
`BlobStore` trait returns (`Payload` has no way to hand over its buffer; `hsearch-object` is outside this task). On `S3Store`
the response body is already copied once into the `Payload`, so a request is copied twice after the network. Against a
real object-store GET (milliseconds) the wrapper's share is small, but that is estimated, not measured on S3. Not profiled with a sampling profiler.

- Memory: nothing here allocates beyond one `Vec` per request, at most `PageCacheConfig::max_request` (and 16 MiB by the
  store's own cap), times the cache's request concurrency. Bounds on resident memory and disk are the cache's
  (`disk_budget`, the pager's budget). Whether a request larger than `max_request` can arise was not tested.
- Each `get_range` blocks its calling thread on the backend future with a std-only `block_on`; `CachedBlobDirectory` already
  prefetches files on its own scoped threads, so no async runtime is entered. A cooperative runtime would need the
  upstream `BlobStore` to be async.
