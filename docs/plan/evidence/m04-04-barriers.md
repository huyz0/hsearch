# M04.04 apply and reader barriers

`provisional`: unit-level, one Linux host, no object store. Nothing calls the barrier module yet: the writer (`crates/index/src/writer.rs`, being changed by another thread) still uses its own snapshot slot, and moving it onto `Replay` and `Readers` is M04.05 and M06.07's. This task adds a module and one `mod` line.

## What exists

`hsearch_index::barrier` (`crates/index/src/barrier.rs`), sans-I/O, positions are slot-prefix lengths:

- **`Replay`**: in-order, exactly-once apply. `begin(seq)` answers `Apply`, `Duplicate` (already applied, skip) or `Gap { expected }`; `done(seq)` advances the contiguous applied prefix and refuses an out-of-order call; `fail()` poisons the cursor, because a failed apply may have partly changed the engine, so recovery builds a new cursor from the durable log.
- **`Readers<R>`**: the committed watermark and the published reader. `publish(reader, covers)` is refused when `covers` is beyond the committed watermark (`Uncommitted`: a write whose persistence failed or has not finished is never searchable), behind the published prefix (`Regress`), or when too many older generations are still pinned (`Pinned`). `commit` never goes back. A reader reports the exact prefix it covers. `pin()` returns the generation as an `Arc`; the fetch round of a query reuses the query's pin, so the documents ranked are the documents fetched, whatever is published meanwhile.
- **Refresh batching without a timer**: `refresh_due(applied)` is true only when `min(applied, committed)` exceeds the published prefix, so a refresh is never due for state that is applied but uncommitted, and many writes between two refreshes become one publication.
- **`Waiters`**: bounded waits for a prefix, released when a published reader covers them (`refresh=wait_for`, M06.07); a full set is refused so the caller answers busy instead of queueing without bound.

## Evidence

- `crates/index/tests/barrier.rs`: 13 tests (ordered apply, duplicate and gap, misuse, poison and rebuild, refusal of an uncommitted cover, no regression, refresh before and after failed persistence, query and fetch sharing a pin across a publish, the pinned-generation bound, a crash between apply and refresh recovering to the durable prefix, a concurrent fetch keeping its generation while another thread publishes 38 more, waiter release, refresh due). Mutations each seen red: the uncommitted check removed (3 tests fail), the pin bound removed, a gap treated as apply, refresh due ignoring the commit, the regress check removed. Clippy `-D warnings` clean.
- Benchmark (`crates/index/tests/barrier_bench.rs`, release, `--ignored`, shared sandbox host, provisional): `pin` 18 ns per call on one thread; 227 to 232 ns per call at 4 and 16 threads (wall time divided by total calls, so about 4.3 million pins per second in total); `publish` 79 ns per call.

## Downsides and limits

- `pin` takes a mutex, which is why contended pins cost 13 times a single-thread pin. At 4.3 million pins per second in total it is far below a query's cost, so a lock-free swap (an extra dependency) was not added; if profiling of a real query path shows it, that is the change.
- Pinned generations are counted, not sized: the bound limits how many readers a slow fetch can keep alive, and each reader's own memory is the engine's.
- The module does not persist anything or touch the engine: the caller must call `commit` only after the slot is durable and must feed `Replay` from the log. It does not enforce that a reader's `covers` is true of the reader it is given; the caller states it.
- Not wired: `refresh=false`, `wait_for` and `true` timing and the visibility target are M06.07; cache and merge integration is M04.05.
