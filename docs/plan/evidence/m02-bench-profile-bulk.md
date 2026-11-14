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
- Open-loop sweep (fixed arrival rate, distinct keys, 32 workers; latency from the scheduled arrival so queueing shows; `completed/s` divides by the time to the last completion; no errors, nothing unstarted): 64 KiB keeps p50 7-12 ms through 500/s, is saturated at 800/s (completes about 715/s, p50 359 ms) and at 1200/s (about 706/s, p50 1.8 s, p99 3.5 s), so about 710 small PUTs/s is the completion limit on this VM and it matches the closed-loop 692-742. 1 MiB (p50 17-22 ms) and 4 MiB (p50 42-46 ms) show no queueing up to the highest rates offered, 120/s and 30/s, which is about 120 MiB/s: no knee was found for large payloads, so 120 MiB/s is a lower bound on this VM and the closed-loop maximum (105 MiB/s) was below it. p99 in the sweep varies run to run by a factor of two or more at low rates (the 64 KiB p99 at 200/s was 17 ms in one run and 130 ms in another), so only p50 and the saturation point are quoted.
- One hot key sustains roughly 78-92 commits/s here. Attempts per cell are attempts/s x 5: about half of attempts conflict at 4 writers (386 of about 770) and 37% at 8 (231 of about 615); 95 of about 575 (17%) at 16, plus 27 other errors. "Other errors" are counted apart from conflicts; the one variant recorded (process-wide list, printed once at the end) is `ResolutionUnavailable`, not attributed per row.
- Matched workloads (16 KiB commits; sparse one writer paced at 50 ms; busy 16 writers on own keys; skewed 16 writers, 90% on one hot key). Uncontended a commit is exactly one PUT at any concurrency (sparse 16/s, busy 358/s). Under the hot key each commit costs about 1.29 PUT, 1.38 HEAD and 0.29 LIST (a LIST is priced like a PUT), throughput falls to 59 commits/s and p99 reaches 3.6 s. Conflicts and other errors are totals over the 5 s cell; attempts/s and commits/s are rates. Hot keys, not writer count, drive cost. Across the two full runs the skewed cell gave 1.19-1.29 PUT and 0.19-0.29 LIST per commit, so the per-commit extra cost has run-to-run spread of that size.
- **Control and direct probe (answers the open ETag question).** The control repeats the contended loop with identical bodies. A direct probe updates from version v1 with identical bytes twice: the second update from the same stale v1 was ACCEPTED, and the 16-writer identical-body loop had 0 conflicts. So on RustFS 1.0.4 an `If-Match` update that rewrites identical bytes is not fenced by the stale version: the content hash, not the version, decides the precondition. The earlier "about 180 commits/s per key, conflicts low" result was therefore this failure mode, not a measurement of a working CAS. Consequence for the commit protocol: a commit body or key must carry a unique per-commit value (for example the new sequence number inside the body), never rely on content-addressed ETag equality for fencing; whether other S3 implementations behave the same is not tested.
- The 958 LISTs and 2423 HEADs in the final counters are whole-run totals across all sections (the contended and skewed cells produce them); only the per-commit columns above are attributed. KMS path accounting needs a real provider and is tracked as M05.08.

Raw output (the open-loop section is from a separate `BENCH_ONLY=open_loop` run after its columns were corrected to count errors and to divide completions by the time to the last completion; the first full run's open-loop table had `done/s` divided by the offered window and could not show failures, so it was replaced):

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

| payload | offered/s | completed/s | errors | p50 ms | p99 ms | unstarted | MiB/s |
|---|---|---|---|---|---|---|---|
| 64 KiB | 200 | 197 | 0 | 7.4 | 129.5 | 0 | 12.3 |
| 64 KiB | 500 | 499 | 0 | 11.8 | 152.3 | 0 | 31.2 |
| 64 KiB | 800 | 715 | 0 | 359.4 | 620.1 | 0 | 44.7 |
| 64 KiB | 1200 | 706 | 0 | 1803.5 | 3499.6 | 0 | 44.1 |
| 1024 KiB | 30 | 30 | 0 | 18.4 | 30.0 | 0 | 30.1 |
| 1024 KiB | 60 | 60 | 0 | 21.6 | 144.8 | 0 | 59.9 |
| 1024 KiB | 90 | 90 | 0 | 17.5 | 73.9 | 0 | 89.9 |
| 1024 KiB | 120 | 120 | 0 | 16.8 | 54.8 | 0 | 119.9 |
| 4096 KiB | 10 | 10 | 0 | 46.0 | 58.5 | 0 | 40.5 |
| 4096 KiB | 20 | 20 | 0 | 42.8 | 65.8 | 0 | 80.0 |
| 4096 KiB | 30 | 30 | 0 | 42.3 | 59.5 | 0 | 119.8 |

requests: RequestCounts { get: 1, put: 39432, head: 2423, list: 958, delete: 0 }
other error variants seen: ["ResolutionUnavailable"]
```
