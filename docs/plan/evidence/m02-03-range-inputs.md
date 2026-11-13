# M02.03 evidence: range-native remote inputs (prototype)

## Finding that shapes the upstream plan

At the pinned lucene-rust rev (`46e280bc`), `lucene_store::Input` is a byte slice (`Owned`, `Mapped`, `Shared`,
`Slice`). Lucene cannot be handed a lazily fetched file, so `CachedDirectory` fetches every file whole and returns
`Input::Owned(Vec<u8>)`. Measured baseline (`tests/remote_file_trace.rs`, 40 documents): opening fetches **all 81
manifest files, 17,168 bytes, once each, before the first query**; term, range, doc-values sort and stored-field
projection queries then fetch nothing. That is the full-file heap allocation this task must remove.

lucene-rust `main` (204 commits ahead of the pin) already has the upstream half: `Input::External(Box<dyn
ExternalBytes>)` with `ExternalBytes::will_need(offset, len) -> Result` (errors reach the caller instead of a page
fault) and a userfaultfd demand-filled pager with a page cache. **No new upstream patch is needed to read by range;
the route is a rev bump** (then `RangeInput` becomes the `ExternalBytes` implementation: `bytes()` backed by the
pager, `will_need` calling `RangeInput::prefetch`). The bump is a workspace `Cargo.toml` change outside this task's
paths and touches every crate, so it is left as the follow-up below.

## What was built (`crates/engine/src/range.rs`)

- `RangeFiles::read_at(name, offset, buf)`: the engine-owned seam, exact fill or error.
- `RangeCache<S>`: fixed-size pages, lengths from the manifest (no HEAD, no LIST), CLOCK eviction with probation,
  adjacent missing pages coalesced into one request up to `max_run_pages`, ranges of any size served in windows so
  memory stays fixed. Everything is allocated in `new` (slab, one run of scratch, per-slot bookkeeping, trace ring);
  `memory_bound_bytes()` reports the total and a test asserts it never moves.
- `RangeInput`: a `Copy` view with `read_at`, `slice`, `prefetch`; every range is bounds-checked with overflow-safe
  arithmetic before any request; a refused or failed read reports an error and never claims success.
- Trace: `Read` (as asked), `Get` and `GetFailed` (as sent), in a bounded ring (overflow is counted, zero disables).

## Validation

- `tests/range_inputs.rs`: byte-exact reads across page boundaries and the short tail; a 40-page file through a
  5-page cache (single read 8x the cache, then 500 random reads) with residency never above the bound; one cold run is
  one GET, a repeat is none, a gap between hits becomes separate runs that re-fetch no hit; injected range errors
  (`Unavailable` and `NotFound`) at every GET position of a mixed workload leave the cache byte-exact and bounded and
  the failed run uncached; bad names, offsets, `u64::MAX` and invalid limits fail before any request.
- Real codecs: all 81 files of a real committed segment (term, doc-values, stored-fields, points, postings, si,
  fnm, segments_N) are read only by range through a cache a quarter of the largest file: the codec header magic and
  footer magic of each are two small reads, and the footer CRC-32 verifies by streaming the whole file in 100-byte
  chunks (not a page multiple). Checksums match for every file.
- Reader paths enumerated: at the pinned rev every path above is reached at open (file-level trace above).
  Byte-range footprints per query inside a file are not observable until the upstream input is adopted; that
  measurement is part of the follow-up.
- Queries through ranges: `queries_over_range_read_files_equal_local_and_fail_cleanly` runs the fixed term, range, doc-values-sort and stored-field queries over files fetched only via the range cache (cache one third of the largest file, evictions observed) and equals the local directory; a range error injected at each of the first six GETs makes open an error, never a partial answer. Lucene still receives whole-file bytes here (pinned `Input`), so this proves the error and equality path, not the no-full-file-heap goal, which needs the rev bump.
- `prefetch` is limited to one request window (`max_run_pages`) so it cannot evict its own pages; tested on a full, warm cache with mixed hits.
- Trace is a capture window (first N accesses kept, rest counted), not a ring.
- Contract: `RangeFiles::read_at` implementors must write every byte on `Ok`; the cache trusts it.
- Vector reader tests: not applicable, no vector capability is advertised.

## Benchmark (provisional: 4 vCPU container, release build, in-memory source so only the cache's own cost shows)

`cargo run --release -p hsearch-engine --example range_bench`, 256 MiB file, 32 MiB cache (working set 8x cache):

| page | run | cold scan | GETs (bytes/GET) | random 4 KiB | amplification | warm 4 KiB |
|---|---|---|---|---|---|---|
| 4 KiB | 16 | 4.2 GB/s | 4096 (64 KiB) | 1.1 us, 0.93 GET/read | 1.8x | 189 ns |
| 64 KiB | 16 | 3.6 GB/s | 256 (1 MiB) | 8.4 us, 0.88 GET/read | 14.9x | 183 ns |
| 64 KiB | 4 | 4.2 GB/s | 1024 (256 KiB) | 9.9 us, 0.88 GET/read | 14.9x | 225 ns |
| 1 MiB | 4 | 3.9 GB/s | 256 (1 MiB) | 150 us, 0.87 GET/read | 224.7x | 242 ns |

Two runs of the same binary differed by up to ~30% in time columns (shared 4-vCPU container); GET counts and amplification are exact.
Hit rate on uniform random reads is 12.5% (= cache/file), as it must be. Warm 4 KiB read with trace off / 4,096-event window / 2.1M-event window (every timed read still pushing events): 258 / 263 / 275 ns, so recording costs roughly 15 ns a read, within run noise.
`memory_bound_bytes()` sums live capacities, including the index: 33-37 MiB for a 32 MiB cache (bookkeeping is about 3% at 4 KiB pages, under 0.1% at 64 KiB+). A test asserts it is unchanged after 50,000 evicting reads (the index is sized at twice the slot count so churn cannot regrow it). Not profiled with a
sampling profiler; the in-memory source removes the network, so these are lower bounds on per-read cost.

## Trade-offs and downsides

- Page size is the lever between GETs and bytes: big pages cut cold-scan GETs 16x but multiply random-read
  amplification (up to 225x at 1 MiB). Postings and doc values want large pages, stored-field and term-dictionary
  probes want small ones; a per-file-kind page size is the likely answer and is unmeasured against real S3 latency.
- Each cold byte is copied twice (source into scratch, scratch into slab) plus once to the caller. Scratch exists so
  a run can span non-adjacent slots; reading straight into the slab would save a copy for single-page runs.
- One lock covers the cache and is held across a fetch, so one slow request stalls every reader, hits included (same v0 limit as
  `CachedDirectory`). Single-flight and per-shard locks are later work.
- RAM-only; an NVMe tier and a cache that survives restart are not here.
- Every page lookup is a `HashMap` probe plus a lock; ~220 ns per 4 KiB warm read is dominated by that and the copy.

## Follow-up (not done here)

1. Bump the lucene-rust pin to a rev with `ExternalBytes::will_need` (workspace change, own task and CI run) and
   implement `ExternalBytes` over `RangeInput`.
2. Record real per-query range footprints with the trace once Lucene reads through it; tune page size per file kind
   against RustFS/S3 latency (M04.02).
