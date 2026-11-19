# M06.16 serving host: one index's durable write path on the lane stack

`provisional`: in-memory object store and local scratch engines on one Linux host. The RustFS test
(`crates/store/tests/host_rustfs.rs`) was compiled here but **not run**: it skips without
`HSEARCH_S3_ENDPOINT` and runs in CI, where `HSEARCH_S3_REQUIRED=1` makes a missing endpoint a
failure. No figure below was measured on RustFS.

## What exists

- **`store::host::Host`** (`crates/store/src/host.rs`). A thread owns the engine and the `Serving`
  writer, opened by `recover_writer_held` (a pin on the base while it is fetched when `Config::pin`
  is set; no LIST). It takes `bulk(ops, Wait)`, `refresh`, `publish` (a base), `evolve` and a stop.
  `reader()` pins the published reader directly through the writer's shared `Readers`, without
  going through the thread, so searches never queue behind writes.
- **Group commit.** Bulks that queue or arrive inside `Config::window` are cut into runs of
  distinct identities of at most `MAX_BATCH_OPERATIONS` (the engine's 16), and a run the engine
  finds too large is halved. Each run is one lane record. A bulk acknowledged `Ok` is durable; with
  `Err` part of it may be durable and it may be sent again.
- **Versions** are server-assigned: the batch at index sequence `s` gives operation `p` the version
  `((s + 1) << 20) + p`, so the next version is a function of the recovered sequence.
- **Bases** on a schedule of batches since the last (`base_every_batches`), counted in
  `HostStats`; a base that fails is counted and never fails a write.
- Supporting changes: `Serving` shares its `Readers` (`readers()`) and can change its dependencies
  (`set_deps`); the engine exports `MAX_BATCH_OPERATIONS`.

## Evidence

`crates/store/tests/host.rs` (10 tests): a searchable bulk is visible with the assigned versions
and a durable one only after a refresh; acknowledged writes recover on a fresh host and a later
write's version is above every earlier one; after every prefix of five batches (a base every
second batch) a restart serves exactly the ledger of acknowledged writes and the next version
rises; a store that refuses a write leaves the bulk unacknowledged and the host serving; a lost,
unresolvable reply breaks the host, a reopen serves the write that was in the log and not the one
refused after it; six concurrent bulks make fewer records than bulks and two operations on one
identity in a bulk become two records in order; five batches with a base every second batch
publish two bases and the restart serves all five; a failed base is counted and the next batch
publishes one; a schema evolved online accepts a document with the new field.

Request counts (in-memory store, `request_counts_...`): one acknowledged record is one write; a
bulk of 20 or 32 operations is two (16 per record); a restart costs the same number of writes
after 1 record as after 40.

Mutations seen red: versions without the `+ 1`; runs without the distinct-identity cut; the
since-base counter not advanced; a broken writer not marked.

## Not done here

- **The node's shared lane.** Each host takes the committer it is given, one lane per index. One
  write log per node needs the per-index cover of M03.09 and lane membership create and delete,
  which moved to M06.21; the first deliverable lane bullet is not claimed.
- Not measured on RustFS (CI only). No byte-based base schedule, only batches.
- The window is a wait for company, so a lone bulk with a nonzero window waits it out.
- Engine limits (documents per index, segments) bound what the host accepts; a refusal is
  `Refused(Limit)` and is not retried.
- A recovered host that cannot refresh fails to open rather than serving without a reader.
- Tests were written after the code, then mutated (disclosed departure from test-first).
