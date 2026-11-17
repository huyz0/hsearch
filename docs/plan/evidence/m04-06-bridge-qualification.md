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
