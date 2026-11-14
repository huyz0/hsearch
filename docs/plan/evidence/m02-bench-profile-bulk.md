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
- The bench's PUT/s and MiB/s columns now count successful PUTs only. The s3 bench is M02.10 work, included here as measurement tooling; its RustFS results are in the section below (added after this note was written).
- The bulk bench workload (~20 byte text) sits in the skipped regime; longer text still pays two analysis passes.

## RustFS bench results (M02.10, M02.04 input)
`provisional`: RustFS 1.0.4 in Docker on the same 4-CPU sandbox VM as the client, 5 s per cell for the contended and matched tables (the payload-size and concurrent-PUT tables are from earlier runs with 2 s cells), `BENCH_SECONDS=5 cargo run --release -p hsearch-s3 --example bench` with the fixed bench below. Relative only.

**Correction (14 November).** An earlier version of this section reported contended compare-and-set as "about 180 commits/s per key, conflicts low". That was wrong: the bench wrote identical bodies after the first update, and the conflict counts went from near zero to hundreds when only the bodies were made distinct. The explanation that fits is that `If-Match` compares the ETag (a content hash) and identical bodies share one, so a stale head still matched and the CAS fenced nothing; a direct probe of that was not run. The bench now writes a distinct body per attempt (`unique_body`) and the numbers below replace the old ones. If that explanation is right, an `If-Match` update that rewrites identical bytes is not fenced by the ETag, so a commit must never rely on the content hash alone (the adapter's token also carries the version id, but the precondition sent is the ETag); that needs its own probe before the format freezes.

| payload | PUT/s | MiB/s | p50 ms | p99 ms |
|---|---|---|---|---|
| 1 KiB | 189 | 0.2 | 5.2 | 7.5 |
| 16 KiB | 213 | 3.3 | 4.5 | 6.6 |
| 256 KiB | 130 | 32.6 | 7.3 | 10.9 |
| 1 MiB | 71 | 70.6 | 13.3 | 19.4 |
| 4 MiB | 24 | 95.2 | 38.4 | 113.9 |
| 16 MiB | 3 | 46.8 | 163.7 | 800.9 |

(Payload-size rows are from the first run with 2 s cells; they do not depend on content and were not rerun, so they carry that duration, not 5 s.)

Concurrent 64 KiB PUTs to distinct keys (second run): 154/s (1 writer), 499 (4), 782 (16), 796 (32); p99 from 13.5 ms to 55.9 ms. Throughput saturates by about 16 writers on this VM.

Contended compare-and-set on one 1 KiB key, distinct bodies:

| writers | attempts/s | winners/s | conflicts | other errors | p50 ms | p99 ms |
|---|---|---|---|---|---|---|
| 1 | 95 | 95 | 0 | 0 | 10.0 | 21.0 |
| 2 | 127 | 99 | 140 | 0 | 10.8 | 70.6 |
| 4 | 169 | 84 | 423 | 0 | 16.4 | 82.7 |
| 8 | 166 | 83 | 431 | 1 | 18.7 | 512.3 |
| 16 | 118 | 93 | 99 | 25 | 10.2 | 2146.0 |

One hot key sustains roughly 80-100 commits/s here regardless of writer count, and about half of attempts conflict at 4 and 8 writers (423 of about 845, 431 of about 830 over the 5 s cell) and about 17% at 16 (99 of about 590, plus 25 other errors). "Other errors" are counted separately from conflicts; the bench records error variants in one process-wide list printed at the end, and the ones seen were `ResolutionUnavailable` (not attributed per row): after a refused condition the adapter resolves "was it my own earlier request?" by listing and probing recent versions within a scan bound, and under heavy contention the hot key's history outruns that bound, so the outcome is reported as unresolvable (ambiguous), not conflict. That is a finding about the adapter's unknown-outcome path under contention, not yet a defect ruled on.

Matched workloads: the same 16 KiB distinct-body commits, same duration, same client and store; only the arrival shape differs (sparse: 1 writer, one attempt per 50 ms; busy: 16 closed-loop writers on their own keys; skewed: 16 closed-loop writers, 90% of attempts on one hot key with real contention, 10% on their own key). Requests per commit are the store's own counters per attempt, including failures.

| shape | writers | attempts/s | commits/s | conflicts | other errors | p50 ms | p99 ms | PUT/commit | HEAD/commit | LIST/commit |
|---|---|---|---|---|---|---|---|---|---|---|
| sparse | 1 | 16 | 16 | 0 | 0 | 11.4 | 39.0 | 1.00 | 0.00 | 0.00 |
| busy | 16 | 362 | 362 | 0 | 0 | 45.8 | 76.1 | 1.00 | 0.00 | 0.00 |
| skewed (counts are totals over the 5 s cell; attempts/s and commits/s are rates) | 16 | 73 | 61 | 52 | 12 | 16.9 | 3596.6 | 1.20 | 1.27 | 0.20 |

Reading: with no contention a commit is exactly one PUT at any concurrency; under a hot key each commit costs about 1.2 PUTs, 1.3 HEADs and 0.2 LISTs because every conflict triggers the identity-resolution list-and-probe (a LIST is priced like a PUT), throughput falls from 362 to 61 commits/s and p99 reaches 3.6 s. Hot keys are the cost driver, not writer count; busy and sparse are linear in commits.

Reading for the design: a PUT has a fixed cost of about 4-5 ms before bytes matter, so small payloads waste the request (16 KiB moves 3.3 MiB/s, 4 MiB moves 95 MiB/s). Packing many documents into one PUT is worth it for request count and cost; 4 MiB was the best size measured and 16 MiB regressed (p99 800 ms). The 26 LISTs and 38 HEADs seen outside the contended sections came from the first (2 s) run and were not attributed to call sites or re-counted. KMS path accounting needs a real provider and is tracked as M05.08.
