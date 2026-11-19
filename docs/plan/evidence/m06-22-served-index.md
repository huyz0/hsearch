# M06.22 serve an index by name, writing a group's runs as one commit

`provisional`: in-memory object store and local scratch engines on one Linux host. The RustFS
host test was not extended here, so no figure below was measured on RustFS.

## What exists

- **`Engine::admit_chain`** (`crates/engine/src/adapter.rs`). Decides a run of prepared batches as
  if each `Fresh` one had been applied before the next: operation, document and history-byte
  counts, new identities, versions per identity, create-only and request identity all see what the
  chain has added. A refused batch leaves no trace. `Engine::admit` is the same check with an
  empty chain.
- **`Serving::write_chain`** (`crates/store/src/serve.rs`). Prepares the batches, decides them
  together, logs the leading ones that pass as one commit (a lane slot, or as few as the slot
  bounds allow), applies them in order, and says in `stop` why the rest were not written
  (`Refused`, `Recognised` or `Failed`). A record in the log after one that failed breaks the
  writer, as a lone record does.
- **The host** (`crates/store/src/host.rs`) writes up to 64 runs of a group per commit. A run
  the log stops at is written alone, split until the operations the engine refuses are told apart
  from the ones it takes, and the runs after it are rebuilt with versions from the records actually
  written, so versions still rise with the sequence and a restart never puts one backwards.
  `Host::bulk_items` returns each operation's result or refusal; `Host::bulk` keeps its earlier
  meaning (a refusal stops the bulk and leaves a durable prefix). `Config` gained `merge` (the
  engine's merge mode) and `searchable_when_empty`; `Host::open` returns a typed `OpenError`.
- **`store::served::ServedIndex`** (`crates/store/src/served.rs`). Create and open by name: the
  index id, shard and lane are derived from the name, so nothing is discovered by LIST. A creation
  that stopped between the lane and its head is finished by creating again; a lane with its head is
  `Exists`; an index with no head is `NotFound`. `bulk` returns a result per operation, `evolve`
  republishes the reader under the new schema (a refresh with nothing new would keep the old
  one), `get_many` with `realtime` refreshes first, and a created index can be searched while empty.

## Evidence

`crates/engine/tests/admit_chain.rs` (3): decisions equal sequential apply for order, version
conflicts and request identity, a bound crossed inside the chain (3 operations allowed, the 4th
refused) is found before anything is applied, a recognised batch adds nothing, and asking changes
nothing.

`crates/store/tests/host.rs` (+3): a refused document in a group fails alone and the rest of its
run and the other bulk are written; 112 distinct identities are 7 records and 7 writes with one
record to a slot and **1 write** when a slot takes 64 records, and a restart over the packed slot
serves every document with the next version above all of them; a refusal in the middle of a packed
run leaves the runs around it written, with strictly rising versions across a restart.

`crates/store/tests/serve.rs` (+1): a chain logs the batches before the first one the engine
refuses and none after it (one write, a recovery finds exactly that batch).

`crates/store/tests/served.rs` (7): created once and a missing index is `NotFound`; acknowledged
writes survive a restart and a real-time read sees a durable write before a refresh; an empty index
answers reads with nothing; a second open fences the first writer; two names do not share a log; a
lane with no head is finished by creating the index; a document the engine refuses fails alone.

Mutations seen red: the chain ignoring earlier batches' operation counts, the chain not recording
earlier batches' identities, `write_chain` not cutting at the first refusal (survived the host
tests, killed by the new `serve.rs` test), the host dropping the runs after a refused one, and one
run per commit (the 7-record packing test fails). Tests were written after the code, then mutated
(disclosed departure from test-first).

## Limits

- Provisional: in-memory store, no timing, no RustFS.
- The engine's bounded profile (128 documents, 32 segments) caps what one engine takes without a
  refresh or a merge; the packing tests stay inside it.
- Admission against the segment bound is conservative inside a chain (every buffered-documents'
  worth of additions counts as a segment); a chain the engine could take after a flush may be cut
  and written in pieces.
- Per-index lane, derived from the name; sharing a lane across indexes is M06.21.
- The node is not on this yet (M06.19), because large field payloads are not written to the log
  until M06.18.
