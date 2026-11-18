# M06.10 rebuild an engine from the recovered log

`provisional`: unit and integration level, in-memory store, a local scratch engine on one Linux host. No RustFS here, so nothing below is claimed for a real object store.

## What exists

- `hsearch_engine::encode_batch(request, mutations)` and `apply_batch(schema, engine, body)` (`crates/engine/src/wal.rs`): the body of one commit-log record. The body is the request identity plus the mutations exactly as `Schema::prepare` takes them (put, create, delete; every value kind, nested objects and joins included). `apply_batch` decodes and goes through `prepare` and `Engine::apply`, so a replayed batch meets every check a live one did and returns the same receipts. A `Shared` text is written as a string.
- Decoding is total: every length is checked against the bytes that remain, trailing bytes, an unknown tag, a bad boolean and a bad version are `Error::Document`, and a body never yields a partial batch.
- `hsearch_store::rebuild::rebuild(publisher, target, dependencies, schema, engine)`: runs recovery and applies each record after the base through `apply_batch`. An engine refusal stops with `RebuildError::Engine{seq, error}`; recovery's refusals pass through as `NotReady`; a record-level delete is `Unsupported(seq)` (batches carry their own document deletes).

## Evidence

- `crates/engine/tests/wal.rs`, 3 tests: a replayed batch gives the same receipts and documents (tag, body, integer, boolean, date) as the live one (put, create with empty/false values, delete of a live and a missing id, nested objects); vector, join and shared-text values replay to the same receipts, document count and stored fields as live (a vector's component values are validated by `prepare` for dimension and finiteness but are not read back); an object nested in an object is refused at encode; every strict prefix of a body, a trailing byte, a wrong version and an oversized count are refused with nothing applied, and the intact body still applies.
- `crates/store/tests/rebuild.rs`, 4 tests, the log written by the real committer: a fresh scratch engine rebuilt from the log alone matches an independent model (never touching the engine) on presence, version and stored value of every identity and on every batch's receipts (outcome, version, id, in order); a stale version and a put under a tombstone's version are each `Conflict`; the first batch's request identity is recognised; an undecodable body stops at its sequence; a record-level delete is `Unsupported`; a missing head is `NotReady(NoHead)`; a head that names a base is `BaseNotOpened`, never a short engine.

Mutations seen red: trailing bytes accepted, a delete decoded as a put, a record-level delete applied, the base check removed, nested-in-nested encode allowed, a boolean always written true, a date tagged as an integer. **One survives and is equivalent in the test:** removing the count-versus-remaining-bytes bound still ends in `Document`, because the first read fails; the bound is what stops a hostile count from reserving memory first, and no test measures that.

## Contract and limits

- **Only batches the engine accepted belong in the log.** Replay turns an engine refusal (a stale version, a create-only collision, a limit) into `RebuildError::Engine` and stops, so a durable-first writer that logs a body before the engine applies it would stop its own recovery at that sequence. Which side of the commit the engine apply sits on is the write path's decision (M06.05); this task states the requirement and tests that a refusal stops at its sequence.
- The engine is discarded on any error (a refused or partial apply can poison it); rebuilding into a non-empty engine is not refused.
- A head that names a base is refused (`BaseNotOpened`); opening an engine on a verified base is M06.11 (engine side) and M06.12 (store side), and spill, external payloads and following a sealed index are M06.13.
- Memory: the receipts of every replayed batch are held, bounded by the caller's record limit; decoding a body reserves roughly sixteen times its size and there is no body-length cap here (the lane's inline record bound is the limit in practice).
- The body format is version 1 and is a new on-log contract; the Java shell does not read it.
- Tests were written after the code, then made to fail by the mutations (disclosed departure from test-first).
