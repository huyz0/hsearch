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

**Correction (14 November).** An earlier version reported contended compare-and-set as "about 180 commits/s per key, conflicts low". That was wrong: the bench wrote identical bodies after the first update, and conflict counts went from near zero to hundreds when only the bodies were made distinct. A direct probe, below, now confirms the cause: `If-Match` compares the ETag (a content hash), so an update with identical bytes from a stale version is accepted and the CAS fences nothing. The earlier invalid run's raw output was not kept; the control section in the raw output below reproduces it deliberately.

Reading, from the raw output below (a later full rerun than the one first recorded here, after the control and open-loop sections were added; every section was rerun, so the numbers differ slightly from the earlier run):
- PUT by size, one writer: about 4.8 ms p50 before bytes matter (1 KiB 204/s, 16 KiB 204/s, 3.2 MiB/s); throughput rises to 96-105 MiB/s at 4 and 16 MiB; p99 is 102 ms at 4 MiB and 187 ms at 16 MiB. Packing many documents into one PUT is worth it for request count and cost; 4 MiB and 16 MiB are close, and earlier runs ordered them differently, so no best size is claimed beyond "megabytes".
- Concurrent 64 KiB PUTs to distinct keys saturate near 16-32 writers (692-742/s).
- Open-loop sweep (fixed arrival rate, distinct keys, 32 workers; latency is from the scheduled arrival so queueing shows): 64 KiB holds p99 under 30 ms up to 500/s, reaches p99 286 ms at 800/s, and is past saturation at 1200/s (p50 1.5 s, p99 2.9 s, all completed only because a drain of one cell length was allowed). 1 MiB holds p50 15 ms, p99 24-82 ms through 120/s (120 MiB/s); 4 MiB holds p50 41-46 ms through 30/s (121 MiB/s). Throughput limit on this VM is therefore about 700-750 small PUTs/s or about 120 MiB/s, and the closed-loop tables above agree with the open-loop knee. `done/s` here is completions over the offered window and equals the offered rate whenever the cell drained; saturation shows in the latency columns, not in `done/s`.
- One hot key sustains roughly 78-92 commits/s here. Attempts per cell are attempts/s x 5: about half of attempts conflict at 4 writers (386 of about 770) and 37% at 8 (231 of about 615); 95 of about 575 (17%) at 16, plus 27 other errors. "Other errors" are counted apart from conflicts; the one variant recorded (process-wide list, printed once at the end) is `ResolutionUnavailable`, not attributed per row.
- Matched workloads (16 KiB commits; sparse one writer paced at 50 ms; busy 16 writers on own keys; skewed 16 writers, 90% on one hot key). Uncontended a commit is exactly one PUT at any concurrency (sparse 16/s, busy 358/s). Under the hot key each commit costs about 1.29 PUT, 1.38 HEAD and 0.29 LIST (a LIST is priced like a PUT), throughput falls to 59 commits/s and p99 reaches 3.6 s. Conflicts and other errors are totals over the 5 s cell; attempts/s and commits/s are rates. Hot keys, not writer count, drive cost. Across the two full runs the skewed cell gave 1.19-1.29 PUT and 0.19-0.29 LIST per commit, so the per-commit extra cost has run-to-run spread of that size.
- **Control and direct probe (answers the open ETag question).** The control repeats the contended loop with identical bodies. A direct probe updates from version v1 with identical bytes twice: the second update from the same stale v1 was ACCEPTED, and the 16-writer identical-body loop had 0 conflicts. So on RustFS 1.0.4 an `If-Match` update that rewrites identical bytes is not fenced by the stale version: the content hash, not the version, decides the precondition. The earlier "about 180 commits/s per key, conflicts low" result was therefore this failure mode, not a measurement of a working CAS. Consequence for the commit protocol: a commit body or key must carry a unique per-commit value (for example the new sequence number inside the body), never rely on content-addressed ETag equality for fencing; whether other S3 implementations behave the same is not tested.
- The 958 LISTs and 2423 HEADs in the final counters are whole-run totals across all sections (the contended and skewed cells produce them); only the per-commit columns above are attributed. KMS path accounting needs a real provider and is tracked as M05.08.

Raw output:

```
cpus: 4, seconds per cell: 5

## PUT by payload size, one writer, distinct keys (batching)

| payload | PUT/s | MiB/s | p50 ms | p99 ms |
|---|---|---|---|---|
| 1 KiB | 204 | 0.2 | 4.8 | 7.4 |
| 16 KiB | 204 | 3.2 | 4.8 | 7.1 |
| 256 KiB | 122 | 30.5 | 8.0 | 13.0 |
| 1024 KiB | 66 | 66.1 | 14.3 | 21.9 |
| 4096 KiB | 24 | 96.5 | 39.6 | 102.0 |
| 16384 KiB | 7 | 104.7 | 149.0 | 186.9 |

## Concurrent PUTs of 64 KiB to distinct keys

| writers | PUT/s | p50 ms | p99 ms |
|---|---|---|---|
| 1 | 143 | 6.4 | 14.1 |
| 2 | 300 | 6.2 | 12.7 |
| 4 | 487 | 7.9 | 14.3 |
| 8 | 631 | 12.3 | 22.6 |
| 16 | 742 | 21.2 | 32.0 |
| 32 | 692 | 45.4 | 69.6 |

## Contended compare-and-set on one 1 KiB key

| writers | attempts/s | winners/s | conflicts | other errors | p50 ms | p99 ms |
|---|---|---|---|---|---|---|
| 1 | 85 | 85 | 0 | 0 | 11.4 | 19.7 |
| 2 | 115 | 90 | 125 | 0 | 10.9 | 75.4 |
| 4 | 154 | 78 | 386 | 0 | 17.6 | 89.2 |
| 8 | 123 | 78 | 231 | 0 | 14.9 | 615.4 |
| 16 | 115 | 92 | 95 | 27 | 10.5 | 1994.0 |

## Matched workloads, 16 KiB commits, compare-and-set on a per-key head

| shape | writers | attempts/s | commits/s | conflicts | other errors | p50 ms | p99 ms | PUT/commit | HEAD/commit | GET/commit | LIST/commit |
|---|---|---|---|---|---|---|---|---|---|---|---|
| sparse | 1 | 16 | 16 | 0 | 0 | 11.3 | 51.1 | 1.00 | 0.00 | 0.00 | 0.00 |
| busy | 16 | 358 | 358 | 0 | 0 | 42.1 | 100.5 | 1.00 | 0.00 | 0.00 | 0.00 |
| skewed | 16 | 77 | 59 | 81 | 13 | 19.0 | 3574.8 | 1.29 | 1.38 | 0.00 | 0.29 |

## Control (expected to expose missing fencing): identical bodies, one hot key

probe: first update from v1 with identical bytes: accepted; second update from the same stale v1: ACCEPTED (stale head not fenced)

| writers | attempts/s | winners/s | conflicts | other errors |
|---|---|---|---|---|
| 16 | 122 | 122 | 0 | 0 |

## Open-loop PUT sweep (fixed arrival rate, distinct keys, 32 workers)

| payload | offered/s | done/s | p50 ms | p99 ms | dropped | MiB/s |
|---|---|---|---|---|---|---|
| 64 KiB | 200 | 200 | 6.5 | 17.5 | 0 | 12.5 |
| 64 KiB | 500 | 500 | 8.6 | 21.5 | 0 | 31.3 |
| 64 KiB | 800 | 800 | 50.2 | 286.2 | 0 | 50.0 |
| 64 KiB | 1200 | 1200 | 1525.4 | 2909.4 | 0 | 75.0 |
| 1024 KiB | 30 | 30 | 16.4 | 29.0 | 0 | 30.2 |
| 1024 KiB | 60 | 60 | 15.3 | 24.0 | 0 | 60.2 |
| 1024 KiB | 90 | 90 | 15.6 | 41.9 | 0 | 90.2 |
| 1024 KiB | 120 | 120 | 15.3 | 81.5 | 0 | 120.2 |
| 4096 KiB | 10 | 10 | 46.3 | 57.8 | 0 | 40.8 |
| 4096 KiB | 20 | 20 | 41.1 | 50.3 | 0 | 80.8 |
| 4096 KiB | 30 | 30 | 41.6 | 112.8 | 0 | 120.8 |

requests: RequestCounts { get: 1, put: 39432, head: 2423, list: 958, delete: 0 }
other error variants seen: ["ResolutionUnavailable"]
```
