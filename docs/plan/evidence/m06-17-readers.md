# M06.17 serving writer publishes readers through the barrier

`provisional`: in-memory object store, local scratch engines on one Linux host. No RustFS here.

## What exists

- **`Serving` owns a `hsearch_index::barrier::Readers<Snapshot>`** (`crates/store/src/serve.rs`). The committed watermark starts at the writer's first sequence and rises with each committed batch. `Serving::refresh(&engine)` cuts the `Replay` cursor, opens an engine snapshot and publishes it with that cut, so a reader covers exactly the batches applied when it was taken (`&mut self` keeps writes out between the cut and the snapshot). `reader()` pins the published generation for a query and its fetch round; `refresh_due()` says whether anything newer than the published reader is applied and committed, so a refresh with nothing new publishes nothing.
- **A failed refresh never withdraws a reader.** `RefreshError` names the step: `Cut` (the cursor failed, so the writer is broken), `Engine` (the engine could not open a snapshot) or `Refused` (the barrier refused the publish; `Pinned` when queries hold too many older generations). In each case the previously published reader is the one `reader()` returns.
- `MAX_PINNED_READERS` is 2, below the engine's limit of four open snapshots so the barrier's own generations cannot exhaust it.

## Evidence

`crates/store/tests/readers.rs` (4 tests).

- A reader covers exactly the applied batches (generation 1 covers 1, generation 2 covers 2), a held reader stays frozen while the writer moves ahead, and a refresh with nothing new returns the same generation.
- The engine cannot open a snapshot (three held beside the published one): the refresh fails with `Engine`, the serving reader still covers its batch and does not see the newer one, and after the extra snapshots are dropped the next refresh publishes it.
- Queries hold two generations: the refresh is `Refused(Pinned)`, the serving reader is unchanged, and after the pins are dropped the next refresh publishes.
- A batch the engine refused after its commit breaks the writer; with an applied batch still due, the refresh is `Cut`, the serving reader is unchanged.

Mutations seen red: the committed watermark not raised after a write (all four fail), the refresh-due guard removed (the no-new-batches case fails). Tests were written after the code, then mutated (disclosed departure from test-first).

## Limits

- Provisional: in-memory store, no RustFS, no query load, no timing.
- **The resolver (M04.03) is not wired here.** The serving writer prepares a batch with the engine (`Schema::prepare`, `Engine::admit`), not with `hsearch_index::resolve::Resolver`, whose updates, upserts and conditions work over a different log record (`hsearch_index::format::Op`). Moving the write path onto it changes what is logged, so it is its own task (M06.20) rather than a part of this one.
- `Refused(Uncommitted)` and `Refused(Regress)` cannot be reached through `Serving` (the watermark and the cut come from the same writer) and are not tested.
- The pin bound is a count, not a time: a query that never drops its handle blocks every later refresh. The caller of `reader()` bounds how long it holds one (M06.16 host).
