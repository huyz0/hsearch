# M04.03 resolved-operation preparation

`provisional`: unit-level, one Linux host, in-memory committed state. Nothing calls the resolver yet: wiring it into the writer and the `_update` and bulk handlers is M06.05 and M21.06 (the writer's apply path is being changed by another thread, so this task adds a module and touches no existing file but `lib.rs`).

## What exists

`hsearch_index::resolve` (`crates/index/src/resolve.rs`), sans-I/O:

- `Resolver::prepare(&impl CommittedState, first_version, &[Request]) -> Prepared`. Requests: `Index`, `Create`, `Delete`, `Update` (partial document, `upsert`, `doc_as_upsert`, `detect_noop`) with `Cond` (`if_seq_no` and `if_primary_term`, both or neither). There is no script field, so a scripted update cannot be expressed (A11).
- What is logged is the resolved whole-document `Op::Put`/`Op::Delete` the writer already understands; replay applies logged ops and never calls the resolver. Preparation reads no clock, random source or network, and takes `&` state, so it cannot change it.
- A refused request logs nothing and consumes no version (`Conflict`, `Missing`, `Exists`, `Invalid`, `Limit`); a `detect_noop` update that changes nothing logs nothing and reports the committed `seq_no`. Applied ops take `first_version + i`, the slot's own rule.
- Items of one batch see earlier items of the same batch (one atomic slot). A dropped (failed-commit) `Prepared` leaves no trace: the next preparation reads committed state only. `Prepared::ensure_current(head)` refuses a preparation once any slot committed after it was resolved, so two batches prepared in parallel cannot both be logged from one stale view.
- `OutcomeStore`: bounded (entry count, oldest evicted) idempotency outcomes by request id, recorded by the caller only after the slot is durable, so a retry returns the original answer.

## Evidence

- `crates/index/tests/resolve.rs`: 16 tests (partial merge, noop and its version accounting, upserts, conditions including a token from a document's previous life, create races and in-batch dependent updates, failed commit leaving nothing, staleness, determinism and replay, invalid shapes, byte and slot-op bounds, outcome store). Mutations each seen red: ignoring `if_primary_term`, `ensure_current` always `Ok`, overlay ignored, `detect_noop` off, slot text cap off. `cargo clippy -p hsearch-index --all-targets -- -D warnings` clean.
- Benchmark (`crates/index/tests/resolve_bench.rs`, release, `--ignored`): 4,096 partial updates of 200-byte-title documents prepare in 3.74 ms per batch, 913 ns per operation, 50 iterations after a warm-up, on a shared sandbox host (provisional, absolute numbers not comparable elsewhere).

## Downsides and limits

- An update copies the document up to three times (read, merge, log). For ordinary documents that is the 913 ns above; a very large text field should use whole-document index (the large-document lane), not a partial update. Memory is bounded: one overlay entry per distinct id, at most 4,096 documents, and a request that would push the batch's logged text past the slot cap is refused with `Limit`.
- `seq_no` is the log version and `primary_term` the writer epoch; mapping them onto the native version token across routing movement and restore (A04) is M06.05 and M07.03's. The outcome store is in memory only: a restart forgets it, and the durable form (slot-carried request ids) is M06.03's.
- `CommittedState` is the contract the writer must satisfy (real-time view, not published readers); no implementation exists yet.
