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
`provisional`: RustFS 1.0.4 in Docker on the same 4-CPU sandbox VM as the client, every cell 5 s, one run, `BENCH_SECONDS=5 cargo run --release -p hsearch-s3 --example bench`, run after the envelope in [m02-10-envelope.md](m02-10-envelope.md) was committed. Relative only; one run, so differences of 10-20% between cells are noise. The raw output is pasted at the end of this section and every number below is read from it.

**Correction (14 November).** An earlier version reported contended compare-and-set as "about 180 commits/s per key, conflicts low". That was wrong: the bench wrote identical bodies after the first update, and conflict counts went from near zero to hundreds when only the bodies were made distinct. The explanation that fits is that `If-Match` compares the ETag (a content hash) and identical bodies share one, so a stale head still matched and the CAS fenced nothing; a direct probe was not run. If that is right, a commit must never rely on the content hash alone (the adapter's token also carries the version id, but the precondition sent is the ETag); that needs its own probe before the format freezes.

Reading, from the raw output below:
- PUT by size: about 4.7-5 ms p50 before bytes matter (1 KiB 203/s, 16 KiB 189/s, 2.9 MiB/s); throughput rises to 90-100 MiB/s at 4 and 16 MiB; the 4 MiB p99 is 160 ms and 16 MiB p99 302 ms. Packing many documents into one PUT is worth it for request count and cost; 4 MiB and 16 MiB are close here and the earlier run ordered them the other way, so no best size is claimed beyond "megabytes".
- Concurrent 64 KiB PUTs to distinct keys saturate near 16-32 writers (706-750/s).
- One hot key sustains roughly 50-100 commits/s here. Attempts per cell are attempts/s x 5: about half of attempts conflict at 4 writers (424 of about 830) and at 8 (246 of about 685, 36%), and 77 of about 560 (14%) at 16, plus 20 other errors. "Other errors" are counted apart from conflicts; the one variant recorded (process-wide list, printed once) is `ResolutionUnavailable`, not attributed per row: after a refused condition the adapter resolves "was it my own earlier request?" by listing and probing recent versions within a scan bound, and under heavy contention the hot key's history outruns it, so the outcome is unresolvable (ambiguous), not conflict. A finding about the unknown-outcome path under contention, not yet ruled on.
- Matched workloads (16 KiB commits, only the arrival shape differs; sparse: one writer, one attempt per 50 ms; busy: 16 writers on own keys; skewed: 16 writers, 90% on one hot key). With no contention a commit is exactly one PUT at any concurrency (sparse 16/s, busy 352/s). Under the hot key each commit costs about 1.19 PUTs, 1.25 HEADs and 0.19 LISTs (a LIST is priced like a PUT), throughput falls to 58 commits/s and p99 reaches 3.7 s. Conflicts/other-errors columns are totals over the 5 s cell; attempts/s and commits/s are rates. Hot keys, not writer count, drive cost.
- The 961 LISTs and 2456 HEADs in the final counters are whole-run totals across all sections (the contended and skewed cells produce them); they were not attributed per call site beyond the per-commit columns above. KMS path accounting needs a real provider and is tracked as M05.08.

Raw output:

```
cpus: 4, seconds per cell: 5

## PUT by payload size, one writer, distinct keys (batching)

| payload | PUT/s | MiB/s | p50 ms | p99 ms |
|---|---|---|---|---|
| 1 KiB | 203 | 0.2 | 4.7 | 7.7 |
| 16 KiB | 189 | 2.9 | 5.1 | 8.4 |
| 256 KiB | 119 | 29.7 | 8.2 | 13.2 |
| 1024 KiB | 66 | 66.2 | 14.8 | 19.3 |
| 4096 KiB | 23 | 90.7 | 41.0 | 159.8 |
| 16384 KiB | 6 | 99.6 | 152.6 | 302.3 |

## Concurrent PUTs of 64 KiB to distinct keys

| writers | PUT/s | p50 ms | p99 ms |
|---|---|---|---|
| 1 | 133 | 6.5 | 20.0 |
| 2 | 282 | 6.4 | 14.3 |
| 4 | 485 | 7.7 | 14.1 |
| 8 | 639 | 12.1 | 21.4 |
| 16 | 706 | 21.6 | 43.3 |
| 32 | 750 | 41.9 | 66.1 |

## Contended compare-and-set on one 1 KiB key

| writers | attempts/s | winners/s | conflicts | other errors | p50 ms | p99 ms |
|---|---|---|---|---|---|---|
| 1 | 50 | 50 | 0 | 0 | 16.7 | 47.8 |
| 2 | 124 | 97 | 137 | 0 | 10.9 | 70.0 |
| 4 | 166 | 82 | 424 | 0 | 16.5 | 78.4 |
| 8 | 137 | 89 | 246 | 0 | 12.0 | 559.2 |
| 16 | 112 | 93 | 77 | 20 | 10.5 | 2281.0 |

## Matched workloads, 16 KiB commits, compare-and-set on a per-key head

| shape | writers | attempts/s | commits/s | conflicts | other errors | p50 ms | p99 ms | PUT/commit | HEAD/commit | GET/commit | LIST/commit |
|---|---|---|---|---|---|---|---|---|---|---|---|
| sparse | 1 | 16 | 16 | 0 | 0 | 11.9 | 46.4 | 1.00 | 0.00 | 0.00 | 0.00 |
| busy | 16 | 352 | 352 | 0 | 0 | 44.1 | 92.2 | 1.00 | 0.00 | 0.00 | 0.00 |
| skewed | 16 | 69 | 58 | 43 | 14 | 17.9 | 3739.4 | 1.19 | 1.25 | 0.00 | 0.19 |

requests: RequestCounts { get: 1, put: 23278, head: 2456, list: 961, delete: 0 }
other error variants seen: ["ResolutionUnavailable"]
exit=0
```
