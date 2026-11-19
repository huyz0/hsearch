# M06.15 publish a base from the serving writer

`provisional`: in-memory object store, local scratch engines on one Linux host. No RustFS here.

## What exists

- **`Serving::publish_base`** (`crates/store/src/serve.rs`). Commits the engine, writes the tombstone list (`LocalFiles::write_tombstones`), publishes the native files and the list as a base covering the applied prefix, and names it in the lane head with `Publisher::activate`. `Serving` now carries a `Replay` cursor that follows each committed batch through its apply, so the base's `Cut` is the barrier's, not a number the caller states; a failed apply fails the cursor and breaks the writer. With nothing committed it is `Nothing`; with a broken writer, `Broken`.
- **The collection boundary** is checked by `Publisher::activate` before the head is touched (M06.04): a base below the boundary is `BaseError::Publish(Swept)` and nothing names the new objects. This task relies on that check; it adds no second one.
- **`recover_writer_held`** (`crates/store/src/rebuild.rs`) takes an optional `Hold`. The pin is registered after the root is verified and before the first file is fetched, checked usable before each file (`Pin::usable` with the caller's skew), and released when the fetch ends, success or failure. A generation a collection has already passed, or an expired pin, is `RebuildError::Pin`; a pin that runs out mid-fetch is `PinLapsed`. `recover_writer` is the unpinned form.

## Evidence

`crates/store/tests/base.rs` (9 tests).

- Three batches written through `Serving`, a base published (covering 3), two more batches, a second base (covering 5, generation +1, replacing the first without the caller naming it): a recovery opens the second base, replays no tail, and matches the independent ledger for every identity. Before any write, `publish_base` is `Nothing`.
- One base then a three-batch tail: recovery reads `from_seq` 2, replays 3, matches the ledger.
- The head's lane cover is exactly one past the index's last record when other records come first (the lane's opening fence and another member's record put the index's first record at slot 2 or later).
- A recovered writer (`Serving::recovered`) publishes before it writes (cover and base taken from `Recovered`: generation 2 covering 3), writes two batches, and publishes again (generation 3 covering 5).
- A broken writer (a lost, unresolvable reply) publishes nothing and the head names no base.
- Pins: a held recovery costs exactly one store write and leaves no pin object (`NotFound`); with the store refusing writes the pin cannot be registered and **nothing is fetched** (the scratch directory is empty), which fixes registration before the first file; a pin that lapses after the first file stops the recovery with `PinLapsed` and is released; a pin with no lifetime is refused before any fetch.

Mutations seen red: the pin not released, the usable check removed, the lane cover without the `+ 1`, the recovered cover dropped, the recovered base dropped. The tombstone list not written makes recovery fail with `MissingTombstones` (M06.12's refusal, covered there).

## Limits

- Provisional: in-memory store, no RustFS, no request counts beyond the pin write.
- **Swept paths are not tested end to end here.** The boundary refusal of `activate` and of `Pins::register` is covered by `gc.rs` (M06.04); `publish_base` and `recover_writer_held` propagate it as `BaseError::Publish(Swept)` and `RebuildError::Pin`, which no test in this task provokes (raising a boundary needs the sweeper and an inventory-capable store).
- Nothing checks that the head still names the base this writer last published or recovered on: a compaction that replaced it in between is not seen, and the next publication's generation (previous + 1) could then fail to rise past the compacted one. A follow-up for the writer wiring (M06.16).
- A pin registered and then not released (a failed release) is not re-registered by a retry with the same id and token: the new expiry gives different bytes and the registration conflicts until the old pin expires. Recovery retries should use a new token.
- The pin protects the fetch, not the later reopen and tail replay, and is checked before each file, not after the last.
- The lane cover is one past the writer's last committed record, correct only for a lane this index owns alone (a shared lane needs the per-index cover of M03.09).
- `BaseError::Cut` and the Swept paths have no test. `Serving` owns the `Replay` cursor now; moving the published reader onto `Readers` and the resolver, and keeping the published reader when a refresh fails, stay with M06.17.
- Tests were written after the code, then mutated (disclosed departure from test-first).
