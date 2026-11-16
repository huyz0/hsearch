# M04.03 resolved-operation preparation

`provisional`: unit-level, one Linux host, in-memory committed state. Nothing calls the resolver yet: wiring it into the writer and the `_update` and bulk handlers is M06.05 and M21.06 (the writer's apply path is being changed by another thread, so this task adds a module and touches no existing file but `lib.rs`).

## What exists

`hsearch_index::resolve` (`crates/index/src/resolve.rs`), sans-I/O:

- `Resolver::prepare(&impl CommittedState, first_version, &[Request]) -> Prepared`. Requests: `Index`, `Create`, `Delete`, `Update` (partial document, `upsert`, `doc_as_upsert`, `detect_noop`) with `Cond` (`if_seq_no` and `if_primary_term`, both or neither). There is no script field, so a scripted update cannot be expressed (A11).
- What is logged is the resolved whole-document `Op::Put`/`Op::Delete` the writer already understands; replay applies logged ops and never calls the resolver. Preparation reads no clock, random source or network, and takes `&` state, so it cannot change it.
- A refused request logs nothing and consumes no version (`Conflict`, `Missing`, `Exists`, `Invalid`, `Limit`); a `detect_noop` update that changes nothing logs nothing and reports the committed `seq_no`. Applied ops take `first_version + i`, the slot's own rule.
- Items of one batch see earlier items of the same batch (one atomic slot). A dropped (failed-commit) `Prepared` leaves no trace: the next preparation reads committed state only. `Prepared::ensure_current(head)` refuses a preparation once any slot committed after it was resolved, so two batches prepared in parallel cannot both be logged from one stale view.
- `OutcomeStore`: idempotency outcomes by request id plus a body fingerprint (`Replay`, `Mismatch` for an id reused with a different body, `Unknown`), bounded by entry count and estimated bytes with the oldest evicted and an over-size entry never kept, recorded by the caller only after the slot is durable.
- The committed head is read before any document, so a slot that commits mid-preparation makes the preparation stale. Ids over 256 bytes (the slot decoder's cap) or empty, whole documents whose values do not match the schema kinds, and a `first_version` too near `i64::MAX` are refused.

## Evidence

- `crates/index/tests/resolve.rs`: 22 tests (partial merge, noop and its version accounting, upserts, conditions on index, delete and update including a token from a document's previous life, create races and in-batch dependent updates, staleness including a commit mid-preparation, invalid shapes and kinds, id and version bounds, byte and slot-op bounds, outcome store count, bytes and fingerprint). Mutations each seen red: ignoring `if_primary_term`, `ensure_current` always `Ok`, overlay ignored, `detect_noop` off, slot text cap off, head read after the documents. Two tests (`a_dropped_preparation_...`, `..._repeatable_...`) hold by construction of the signatures (`&` state, no clock or random parameter); they pin the contract rather than catch a regression. `cargo clippy -p hsearch-index --all-targets -- -D warnings` clean.
- Benchmark (`crates/index/tests/resolve_bench.rs`, release, `--ignored`): 4,096 partial updates of 200-byte-title documents prepare in 3.1 to 3.7 ms per batch, 754 to 913 ns per operation (two runs), 50 iterations after a warm-up, on a shared sandbox host (provisional, absolute numbers not comparable elsewhere).

## Downsides and limits

- An update copies the document up to three times (read, merge, log). For ordinary documents that is the 913 ns above; a very large text field should use whole-document index (the large-document lane), not a partial update. Memory is bounded: one overlay entry per distinct id, at most 4,096 documents, and a request that would push the batch's logged text past the slot cap is refused with `Limit`.
- `seq_no` is the log version and `primary_term` the writer epoch; mapping them onto the native version token across routing movement and restore (A04) is M06.05 and M07.03's. The outcome store is in memory only: a restart forgets it, and the durable form (slot-carried request ids) is M06.03's.
- Differences from OpenSearch, all explicit refusals: an update with a condition on a missing document is `Conflict` (OpenSearch's document-missing answer is unconfirmed here); an empty `doc` is `Invalid` rather than a no-op; `doc_as_upsert` needs every schema field; external versioning does not exist (PR-47).
- `CommittedState` is the contract the writer must satisfy (real-time view, not published readers); no implementation exists yet.
