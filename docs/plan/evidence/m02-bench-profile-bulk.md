# M02 evidence: bulk-path bench, profile and first optimisation

All numbers are `provisional`: one sandbox VM, in-memory store, not a reference machine. They are relative, not absolute.

## What was measured
- `crates/node/examples/bulk_profile.rs`: `Node::bulk` on `MemoryStore`, profiled with `valgrind --tool=callgrind` on a release build with `CARGO_PROFILE_RELEASE_DEBUG=1`.
- `crates/s3/examples/bench.rs`: PUT size sweep, concurrent PUTs and contended CAS against RustFS (run with `scripts/rustfs.sh up`).

## Finding and change
Profile showed `Schema::document` analysing every text field twice for term counting. The change counts analysed terms only when the raw text bytes exceed `MAX_TERMS` (a text of at most `MAX_TERMS` bytes cannot yield more terms than that bound), preserving the limit. Measured effect on the bulk profile: about +13% documents/s (provisional).

Caveat: text longer than 128 bytes is still analysed twice (count, then index); removing that needs the analysed terms passed through, a later task.

Test: `the_term_bound_counts_analyzed_tokens_not_bytes_across_all_text_fields`.

## Next profile targets (not done here)
- serde_json `Value` DOM in the node, about 8-12%.
- per-field `String` copies in `adapter::Operation`, about 3%.
- per-bulk refresh cost, about 14%.

## Notes from review
- Short text (at most `MAX_TERMS` bytes) no longer surfaces an analyzer failure at prepare time; it surfaces at the native commit. Longer text keeps the prepare-time error.
- The boundary test cannot observe the skip itself (behaviour is identical); it pins the 128/129-byte boundary.
- The bench's PUT/s and MiB/s columns now count successful PUTs only. The s3 bench is M02.10 work, included here as measurement tooling; no result from it is recorded in this PR.
- The bulk bench workload (~20 byte text) sits in the skipped regime; longer text still pays two analysis passes.

## RustFS bench results (M02.10, M02.04 input)
`provisional`: RustFS 1.0.4 in Docker on the same 4-CPU sandbox VM as the client, 2 s per cell, `cargo run --release -p hsearch-s3 --example bench`. Relative only.

| payload | PUT/s | MiB/s | p50 ms | p99 ms |
|---|---|---|---|---|
| 1 KiB | 189 | 0.2 | 5.2 | 7.5 |
| 16 KiB | 213 | 3.3 | 4.5 | 6.6 |
| 256 KiB | 130 | 32.6 | 7.3 | 10.9 |
| 1 MiB | 71 | 70.6 | 13.3 | 19.4 |
| 4 MiB | 24 | 95.2 | 38.4 | 113.9 |
| 16 MiB | 3 | 46.8 | 163.7 | 800.9 |

Concurrent 64 KiB PUTs to distinct keys: 173/s (1 writer), 570/s (4), 866/s (16), 891/s (32); p99 rises from 10.6 ms to 52.6 ms. Throughput saturates by about 16 writers on this VM.

Contended compare-and-set on one 1 KiB key: winners/s 112 (1 writer), 177 (2), 181 (4), 178 (8), 157 (16); conflicts stay low (15 of 164 attempts/s at 16 writers), no other errors. One key sustains roughly 180 commits/s here, so a hot index is bounded by CAS round trips, not by contention.

Reading for the design: a PUT has a fixed cost of about 4-5 ms before bytes matter, so small payloads waste the request: 16 KiB moves 3.3 MiB/s, 4 MiB moves 95 MiB/s on the same store. Group commit that packs many documents into one PUT is therefore worth it for request count and cost; 4 MiB is the best size measured, 16 MiB regressed (p99 800 ms). The whole run issued 10,200 PUTs, 32 HEADs, 26 LISTs and 1 GET. The 26 LISTs were not attributed to a call site in this run.
