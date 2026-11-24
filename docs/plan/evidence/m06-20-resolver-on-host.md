# M06.20 resolve updates, upserts and conditions on the serving write path

`provisional`: in-memory object store and local scratch engines on one Linux host.

## What exists

- **`Host::resolve`** (`crates/store/src/host.rs`) and `ServedIndex::resolve`. The writer thread
  takes a list of `hsearch_index::resolve::Request` (index, create, delete, partial update, upsert,
  `doc_as_upsert`, `detect_noop`, `if_seq_no`/`if_primary_term`), resolves it once with the M04.03
  `Resolver` against an engine view of the committed documents, and writes the resolved whole
  document puts and deletes through the same group-commit path as a bulk. The log holds only puts
  and deletes, so recovery never calls the resolver.
- A version in an item is the version the log assigned (the engine's), and the primary term is the
  shard incarnation, so a client's condition from an earlier answer matches after a restart.
- A refused request (`Missing`, `Exists`, `Conflict`, `Invalid`, `Limit`), a delete of a missing
  document and a detected no-op write nothing and report in their own item. An engine refusal of a
  logged operation becomes `Rejected(Limit|Invalid)` in that item.
- **Retries**: with a `RetryKey` (request id and body fingerprint) the host remembers the answer in
  an `OutcomeStore` (4,096 requests, 4 MiB) and a retry gets it back unchanged; the same id with
  another body is refused (`Invalid`).

## Evidence

`crates/store/tests/served.rs` (+6): update, upsert, failing and passing conditions, no-op,
create-exists and delete-not-found resolve against committed state and take rising versions; the
resolved writes recover from the log and a recovered version still satisfies its condition; a retry
returns the first answer after another write moved the document, and a reused id with another body
is refused; a partial update is refused on an index with a vector field; a mixed batch pairs each item with what the log assigned and refuses a repeated id; a retry still honours its own `Wait::Searchable`.

Added after independent review: the retry's wait, one request per id, and the resolver's engine view dropped before the write (it holds one of the engine's reader reservations). Mutations seen red: the retry's wait, the repeated-id guard, the item version taken from the receipt, the outcome store's record, and the
vector guard (the last two had a weak first test, strengthened until it failed). The new tests
were written after the code (disclosed departure from test-first).

## Limits

- The remembered answers are in memory: a retry after a restart is resolved again, so an update
  that already took effect is applied a second time (a whole-document index is the same
  document). Persisting the request id in the lane record is the fix and is not done here.
- The committed view carries no vector values, so a partial update on an index with a vector field
  is refused instead of dropping the vector.
- An id named twice in one batch is refused (`Invalid`) before anything is written, because the
  resolver would resolve the second against a version and document it only assumed.
- A resolve whose write fails part way (a store failure after earlier runs became durable) returns
  the error and records no answer, so a retry is resolved against the new state: a create can then
  answer `Exists` for a write the first attempt made. The same durable-prefix rule as `bulk`.
- Committed documents carry the host's current incarnation as their primary term, not a stored
  one; the incarnation is a constant 1 today, so a restart cannot change it. A change to that
  needs the term stored with the document.
- The node's `_update` and `create` routes are still refused (M06.05 to M06.08 wire them).
