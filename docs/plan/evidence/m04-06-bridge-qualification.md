# M04.06 engine bridge qualification

`provisional`: one Linux host, in-memory object store (no RustFS: no Docker daemon here), shared machine. Relative numbers only.

## What was run

- **Release-profile engine tests**: `cargo test -p hsearch-engine --release`, every target green (the bridge contract, Java-fixture differential cases, analyzer and scoring parity, lifecycle and reopen, remote and range suites, the new corpus below).
- **Differential, analyzer, scoring, float parity**: unchanged from M04.01 to M04.04: `adapter_java_supplement.rs`, `adapter_public.rs`, `native_feasibility.rs` and the fixtures rendered by `render_adapter_fixtures.py` from hash-frozen Java oracle cases; open, update, delete, merge and restart are in `adapter_lifecycle.rs`, `adapter_checkpoint.rs`, `cache_lifecycle.rs`. No oracle was regenerated.
- **Corruption corpus** (new, `tests/corruption_corpus.rs`): every file of a committed index is truncated (half, footer, empty), has its footer zeroed and has one byte flipped at five offsets: 153 damaged copies.

## Finding: open does not recompute checksums

With open and search alone (as in Lucene), **two of the 153** damaged copies opened and gave **different answers with no error**: a flipped byte in the points file (`.kdd`, range query) and in a doc-values file (`.dvd`). Open verifies each file's header and footer shape, not its CRC; Lucene's `CheckIndex` does that. A wrong answer from a flipped bit is not acceptable for a store whose source is a blob.

Added `verify_files` and `verify_directory` (`integrity.rs`): recompute the CRC-32 of each named file against its footer. With verification, **all 153 are refused, none answers wrongly** (the corpus asserts this, and asserts that open alone is fooled at least once, so the corpus cannot go vacuous). Mutation: with the CRC check removed the corpus fails on those two files.

## Downsides and limits

- `verify_files` reads each file whole: one GET per file on a remote directory, plus a CRC pass (multi-core above a size threshold in lucene-rust). It is therefore **not on the open or query path**. Run it where a file enters the store (publish, ingest), after any repair, and in a scrub; wiring it into publish is M06's. Until then open-time integrity of a remote file rests on the object store's version token and, with encryption, the M05.03 block authentication.
- Same-length damage in a file the CRC does not cover (none known: every native file carries a footer) would not be found.

## Local against remote benchmark

`ranged_snapshot.rs::bench_local_vs_remote` (`--release -- --ignored --nocapture`): 20,000 documents, 387 KB of native files (small by design, so the whole corpus is 33 requests), in-memory backend with a fixed per-request sleep.

| path | open | first answers (3 queries) | requests / bytes | warm answers p50 / p99 |
|---|---|---|---|---|
| local directory | 0.43 ms | n/a | n/a | 331 µs / 532 µs |
| remote, 0 ms request | 5.2 ms | 0.57 ms | 33 / 387,159 | 316 µs / 497 µs |
| remote, 3 ms request | 13.6 ms | 0.57 ms | 33 / 387,159 | 365 µs / 471 µs |
| remote, 20 ms request | 65.8 ms | 0.81 ms | 33 / 387,159 | 420 µs / 816 µs |

Cold open plus first query is **3 sequential round trips** at 3 ms and 20 ms (the 0 ms row's 31 is only requests that did not overlap in time, since nothing sleeps). Open time is about 3 round trips plus 1 to 5 ms of work. A warm remote query is within about 0 to 27% of local at p50 (noise-level at 0 ms). **Not shown by this corpus**: range behaviour at size (each file is a few pages, so everything is fetched); the 333 MiB, 200k-document RustFS numbers are in [m04-02](m04-02-remote-range-input.md). The remote path never materializes whole files beyond the request count above (`heap_budget` bounds it), and a store without demand paging reads small files whole by design.

## Adapter review

Frozen supported contract: the 17-row feature inventory and capability gates of M04.01 are unchanged and tested; unsupported capabilities stay refused with typed errors. No runtime crate bypasses the adapter (M04.01 gate). Independent diff review for this task is recorded on the PR.

## Milestone review (independent) and where its findings are carried

An independent milestone review of M04 judged the third definition-of-done bullet (committed-only readers with pinned generations and deterministic replay on the **serving path**) not met: `hsearch_index::barrier` and `resolve` are proven by their own tests but the writer (`crates/index/src/writer.rs`) does not use them yet, and its `refresh` takes the published reader before the new one exists, so a failed refresh empties the slot. M04.03 and M04.04 recorded that wiring belongs to M06; it is now written into **M06.05**'s deliverables so it cannot be lost. Likewise `verify_files` is now in **M06.02**'s deliverables (publish-time integrity). M04 stays done because its tasks deliver the primitives, the contract and the qualification, and the plan assigns the integration to M06; this note is the visible record that the bullet is met at module level only.

Also found, not fixed here: `crates/engine/CAPABILITIES.md` still carries pre-M04.02 "remain M02.03" text and the old Java-oracle revision (`46e280bc`), the Java differential fixture was not re-recorded at the current pin, no Java-written old-format index reopen is evidenced, `RangeCacheMetrics` is not connected to `hsearch-observe`, the `userfaultfd`-off memory test has no evidence line, and nothing has run at the 1M-document gate scale. These are open qualification gaps for M06.06 and the M14/M17 qualification tasks.
