# M06.19 move the node onto the serving host

`provisional`: in-memory object store and local scratch engines on one Linux host. The RustFS
suites and the 100k-document kill -9 e2e (`scripts/e2e.sh`) were not run here; CI runs them.

## What changed

- `hsearch-node` holds one `ServedIndex` per index (`crates/node/src/node.rs`), created and opened
  by name, with the lane derived from the name; the node's bulk returns each operation's own
  refusal, evolve republishes under the new schema, and search reads the published snapshot.
- `ServedIndex` opens its host with the payload sink (`ObjectPayloads`), so a batch over
  `MAX_INLINE_RECORD_BYTES` is staged as a digest-verified object before its record is logged.
  The chain writer stops at such a batch (`ChainStop::Alone`) and writes it by itself.
- A real-time read (`Host::read_latest`) snapshots the engine on the writer thread and reads the
  ids from it without publishing a reader, so a get sees an acknowledged write that is still not
  searchable, as before. It had been a refresh, which made the write searchable.
- Test support: `Counting::sharded(n)` spreads keys over several memory stores for the
  1,000-index mapping-sharing test, since one `MemoryStore` caps at 4,096 objects and an index now
  costs several (lane, head, mapping reference). The same assertions hold: 100 mapping objects, each
  written once, 100 requests on them.
- The legacy-mapping test now builds the index's lane with `ServedIndex::create`; the inline
  `{index}/mapping` it exercises is unchanged.
- `tools/workspace.py`: `hsearch-node` may depend on `hsearch-store`.

## Evidence

Run here: `cargo test -p hsearch-store` and `cargo test -p hsearch-node` (lib and tests), all
passing, including `large_fields` (6) which needs the sink and `realtime_get` (9) after the
read-only read path; `cargo clippy --workspace --all-targets -- -D warnings` clean.

Added after independent review: a real-time read on a broken host answers `Broken` instead of a
stale view; `crates/store/tests/served.rs` (+1) writes small, over-bound and small documents in one
bulk, drops the index with no refresh, reopens and reads all three (the over-bound one through its
payload object); the real-time test now asserts the read published nothing. Mutation seen red:
the over-bound check removed (the new test fails).

## Limits

- The v0 writer stays in `hsearch-index`: `crates/node/tests/mappings.rs` still builds v0 indexes
  to plant a lying and a dangling mapping reference. Removing it is for when those tests move.
- An index written by the old v0 layout is not opened by this node; there is no deployed data.
- Sweep's orphan pass (`orphan_grace_ms`, off by default, never run by the node) would delete a
  payload that only the log names: M06.23.
- Each index has its own lane until M06.21.
- Names are passed to `index_id` as the node holds them; tenant-qualified names are the caller's.
