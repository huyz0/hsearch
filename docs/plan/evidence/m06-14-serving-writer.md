# M06.14 log-first serving writer

`provisional`: in-memory object store, local scratch engines on one Linux host. No RustFS here.

## What exists

- **`Engine::admit(&PreparedBatch) -> Checked`** (`crates/engine/src/adapter.rs`). Every refusal `Engine::apply` makes before it changes anything (request recognition, limits, create-only and stale versions, the segment bound) now lives in one `precheck`, which `apply` also runs, so the two cannot disagree. `admit` runs it and changes nothing: `Fresh`, or `Recognised(receipts)` for a request already applied with the same operations.
- **`store::serve::Serving`** (`crates/store/src/serve.rs`). `write(schema, engine, request, mutations)` prepares against the schema, admits, and only a `Fresh` batch is encoded (`encode_batch`), committed through the `Committer` and then applied. A recognised request is answered with its receipts and is not written again. Refusals (`Refused`) and certain store refusals (`Rejected`) write nothing and spend no sequence. A lost, unresolvable reply (`Unknown`), a taken slot position (`Fenced`, which may be our own) or a native failure in the apply after the commit leaves the writer `Broken`: it refuses everything until the index is recovered from the log.
- `hsearch-store` now depends on `hsearch-commit` (layer table in `tools/workspace.py`).

## Evidence

`crates/store/tests/serve.rs` (4 tests) and `crates/engine/tests/admit.rs` (1).

- Accepted and refused batches interleaved (a stale version, a batch whose second mutation is stale, a repeated request): the live engine and a recovery of the log into a fresh engine agree on every identity's version and tag, and the recovery applied exactly the two accepted batches. Had a refused batch been logged, the replay would have stopped with `Engine { seq, .. }`.
- A store that refuses the record (`allow` spent): `Rejected`, the engine and the sequence are unchanged, and after the store returns the same batch is written at the same sequence; recovery agrees.
- A lost reply the store cannot resolve: `Unknown`, the live engine did not take the batch, the writer is broken and refuses the next write, and a recovery of the log serves the batch.
- A schema evolved between the commit and the apply (a dependency view that evolves it during the commit): the batch is in the log, the apply refuses it, the writer is `Broken` and refuses the next write, and a recovery serves the batch.
- `admit` leaves nothing searchable, recognises the same request with the original receipts, refuses a different body under it and a stale version exactly as `apply` does.

Mutations seen red: the admit call removed from the writer (a refused batch is logged: the ledger test fails), the broken flag not set on `Unknown` and not set after a post-commit apply failure (the next write is not refused), the stale-version comparison weakened in the shared precheck (`admit` test fails). The recognised-request branch has no mutation that compiles on its own (the match is exhaustive); its behaviour is covered by the repeated-request assertion.

## Limits

- Provisional: in-memory store, no RustFS, no timing or request counts.
- **An admitted batch is applied only if nothing else uses the engine in between.** A schema evolution, a direct `apply` or a native flush crossing the segment bound between `admit` and `apply` makes the apply refuse a batch already in the log (the schema case is tested; the segment case is not). The writer then reports `Broken`. Because replay runs the same checks, a segment-bound refusal could also stop a later recovery at that record; whether replay's inline merging avoids it is unverified. `Serving` documents that it must be the engine's only user.
- The `Rejected(Fenced)` and `NotAttempted` branches are not covered by a test.
- An engine that reports `Poisoned` or `Native` at admit makes the writer `Broken`; the dependency set is fixed for the writer's life, so a changed dependency means `Rejected(Stale)` until it is rebuilt.
- One record per batch and one writer per index; no coalescing of concurrent writers (M06.07) and no wiring into the node (M06.16).
- `Serving` takes the next sequence from the caller (`Recovered::applied` after a recovery); the node wiring in M06.16 supplies it.
- Tests were written after the code, then mutated (disclosed departure from test-first).
