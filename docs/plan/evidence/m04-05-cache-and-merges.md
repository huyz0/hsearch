# M04.05 cache budget, pins, damage repair and merge admission

`provisional`: unit and integration level, one Linux host, no object store in these tests (the cache sits over an in-memory `RemoteFiles`).

## What exists (`hsearch-engine`)

- **`DiskBudget` / `DiskReservation`** (`budget.rs`): one finite local-disk budget, shared by cache fills and merges. `reserve` is a compare-and-swap loop that never passes the cap and refuses with `Error::Limit` without changing anything; dropping a reservation returns its bytes.
- **`CachedDirectory::with_budget`**: every cached file holds a reservation until it leaves the cache. A fill that does not fit evicts the oldest unpinned file and retries; a file that still cannot fit, or whose cache write fails (disk full, unwritable directory), is served from the fetched bytes without being cached (`uncached()` counts them). A read never fails because the local disk is full.
- **`CachedDirectory::pin` / `CachePin`**: a pinned file is skipped by eviction and by `evict_all` until the pin drops. Readers do not need a pin to stay valid: an opened file's bytes are owned by its reader, so eviction cannot invalidate a live snapshot (tested).
- **Damage repair**: a cached file that is missing or whose length differs from what was cached is dropped and refetched (`repaired()` counts). A fetch error from the source is returned as an error: nothing turns a failing or vanished source into a silent miss.
- **`cache_namespace(domain, version)`**: the page-cache namespace for `Snapshot::open_blob`, length-prefixed so two (domain, version) pairs never encode alike; with the manifest version already in lucene-rust's page key (M04.02), pages are keyed by domain, version, file and length.
- **`MergeScheduler` / `MergeTicket`** (`maintenance.rs`) and **`Engine::merge_scheduled`**: at most N merges at once, each reserving disk equal to the bytes the directory holds now (a merge writes its output before freeing inputs); refused with `Busy` (no slot), `Limit` (no room) or `Unsupported` (shut down) before any native work. `shutdown` refuses later admissions and `wait_idle` returns when running merges end. A ticket's drop returns slot and bytes even after a failed merge.

## Evidence

- `crates/engine/tests/cache_lifecycle.rs` (5 tests, real engine segments) plus unit tests in `budget.rs`, `maintenance.rs` and `range.rs`; full `cargo test -p hsearch-engine` and `clippy --all-targets -D warnings` green.
- Mutations each seen red: eviction ignoring pins, the cache-hit length check removed, the reservation dropped instead of held, the merge slot limit removed. The tests were written after the code and then made to fail by these mutations, not before it.

## Downsides and limits

- A cache-hit open adds one length comparison (O(1)); a fill adds one atomic CAS per file. Neither is on a per-query path and neither was benchmarked separately.
- Damage detection is length and presence only. A same-length bit flip in the cache is caught by Lucene's own footer checksums and surfaces as an error, not a refetch; `evict_all` then recovers.
- The uncached fallback holds a whole file on the heap for that read (bounded by the file, and by `heap_budget` on the range path).
- `merge_scheduled` still runs the merge under the engine's state lock, as `merge` does; moving merges off that lock is M21.10. The reservation is an upper bound (inputs' size), not the output's measured size.
- `PageCache` (the range path) keeps its own disk budget; sharing a `DiskBudget` with it needs a lucene-rust change and is not done here.
