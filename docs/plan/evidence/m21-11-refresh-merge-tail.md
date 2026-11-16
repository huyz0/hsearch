# The 1.37 s refresh stall: cause, fix, numbers

Provisional: 4-CPU cloud container, in-memory object store (no network), one node, release build, local scratch disk. Relative numbers only. Reproduce: `cargo run --release -p hsearch-node --example visibility_tail -- 3000 100 250 <auto_merge_mb>` (0 is the old behaviour).

## What was seen

[m21-10](m21-10-rustfs-measurements.md) found one `Wait::Searchable` call of a 100-document batch taking 1.37 s on RustFS, in two runs, against a 20 ms median.

## Cause

It does not need a network: it reproduces on the in-memory store, at the same call index every run. The engine's writer had no merge policy, so every refresh added one segment and nothing merged them. At 254 refreshes the segment cap (256) was reached, the write was refused with `Limit`, and the index writer committed and ran `force_merge(1)`, one blocking merge of the entire index, on that write. The stall is at the 255th refresh (call index 255, 509, 763 in the runs: every 254 after the first merge), which matches the engine's own check (`segments + 2 > 256` refuses the 256th write; test `the_segment_cap_is_reached_without_auto_merge_and_not_with_it` shows 255 writes admitted without merging and all 300 with it). It recurs and grows with the index: the stalled call took 1.4 s with 25,000 documents, 2.3 s with 280,000 and 3.2 s with 509,000 (batches of 1,000), so the visibility p99 target is lost at a few hundred thousand documents on any steady refresh load, not only once.

Because the cost is per call index, not per document, the same stall shows with batches of 10, 50 and 100 (documents before the stall scale with the batch).

## Fix

An engine setting `auto_merge_mb` (`Limits`, node flag `--auto-merge-mb`, default 4 (was 16, see the follow-up below), 0 turns it off. The sections above were measured at 16: their 8 MiB threshold is 2 MiB at 4, and their ratios against off (worst call 8 to 12x, bytes +19%, +60%, +84%) are for 16, not 4): the writer gets a tiered merge policy whose largest merged segment is that many MiB. The policy runs inside the refresh's flush and folds small segments as they appear, so segments stay few and no refresh merges more than a few small ones. The policy only merges segments below half the cap (8 MiB at 16): a larger segment is left alone, and the forced merge at the cap stays as the backstop. The stall is therefore deferred, not removed, for workloads whose single refresh writes 8 MiB or more (fat documents, or very large batches): after about 250 such refreshes the old whole-index merge returns. The runs below only cover small documents; a larger `--auto-merge-mb` moves the threshold but made the tier stalls longer (below). The engine's own default stays 0 (the research profile and every existing test pin the old behaviour); the node turns it on.

## Numbers

3,000 `Wait::Searchable` bulks of 100 documents (300,000 documents), 3 runs each:

| | worst call | p50 | p99 | whole run | process bytes written | search p50 / p99 after |
|---|---|---|---|---|---|---|
| off (old) | 2.25 to 2.36 s, call 2795 | 14.8 to 17.7 ms | 29 to 38 ms | 65 to 76 s | 116 MiB | 2.5 to 3.9 / 3.9 to 8.5 ms |
| 16 MiB | 0.26 to 0.31 s | 9.1 to 10.8 ms | 35 to 37 ms | 31 to 38 s | 138 MiB | 2.0 / 2.8 to 4.2 ms |

At 25,000 documents (the m21-10 size) the worst call went from 1.4 to 1.7 s to 0.12 to 0.13 s (first sweep, one run). Larger batches, one run each (the node refuses bulks over 5,000 items, and the index holds at most 1,000,000 identities):

| batch x calls | setting | worst call | p50 | p99 | bytes written |
|---|---|---|---|---|---|
| 1,000 x 800 | off | 4.66 s (also 2.97 s) | 38.8 ms | 88.5 ms | 119 MiB |
| 1,000 x 800 | 16 | 0.34 s | 26.6 ms | 170 ms | 191 MiB |
| 1,000 x 800 | 64 | 0.69 s | 29.6 ms | 160 ms | 219 MiB |
| 1,000 x 800 | 256 | 0.61 s | 36.0 ms | 186 ms | 219 MiB |
| 4,000 x 240 | off | 4.62 s (also 2.72, 3.23 s) | 158 ms | 2,718 ms | 127 MiB |
| 4,000 x 240 | 16 | 0.40 s | 144 ms | 366 ms | 234 MiB |

At 300,000 documents in batches of 100, 64 MiB left a 0.38 to 0.40 s stall against 0.26 to 0.31 s at 16 MiB, and at 1,000 per batch 64 and 256 were worse than 16, so the first default was 16; the follow-up below moved it to 4. Note the cost: with 1,000 or 4,000 documents per refresh the bytes written rose 60% and 84%, and p99 rose from 88 ms to 170 ms at 1,000 per batch (off had only the stalls above p99).

## Trade-offs and downsides

- The worst call fell 8 to 12x and, with small batches, the median and whole-run time roughly halved (fewer segments to flush against and search). p99 did not improve with small batches (about 36 ms, against 29 to 38 ms off) and got worse with batches of 1,000 (170 against 88 ms): the merges move cost from one huge stall to small stalls spread over many calls.
- Bytes written rose 19% (138 against 116 MiB for 300,000 documents; the process counter includes the in-memory store's copies, so this is a ratio, not an absolute). With an object store, merged segments are uploaded again at publish, so the uploaded bytes can rise too (the counter here is the process's total writes, not PUT bytes, so that share is not measured). PUT counts per bulk are unchanged (merging is local); the publish path was not measured against RustFS.
- A 0.26 to 0.40 s stall remains where a larger tier merges; the sizes above show it is bounded by the tier, not by the index, for small documents. The example prints no segment or merge counts, so how often a tier merge happens was not measured.
- The merge runs inline on the write path, inside the engine's refresh while the engine's state lock is held, so it blocks every other write and refresh of that index for its duration. A background merge would hide it, but needs a scheduler and a shared lock with refresh; not done.
- Segment objects that a merge replaced and that an earlier publish already uploaded stay in the store: nothing deletes them yet (the orphan/GC work is separate), so more merging means more dead objects and bytes.
- The default is on for the node, from one workload family (small documents, in-memory store, no deletes or updates). `--auto-merge-mb 0` restores the old behaviour for a deployment where the extra bytes matter more than the stall. A per-index override needs an index settings API that does not exist yet.
- Not measured: remote (range-read) segments, concurrent writers, deletes and updates (the merge also reclaims deleted copies), memory during a merge.
- There is no per-index setting yet: only the node-wide flag. The setting is part of the engine schema's equality, so a checkpoint reopens under the value it was taken with; within one node process the flag is constant.

## Tests

`without_auto_merge_every_refresh_adds_a_segment` (40 refreshes, 40 segments, all documents found by term: pins the old behaviour), `auto_merge_keeps_segments_few_and_an_open_snapshot_readable` (at most 20 segments, every document still found through the postings after the merges, a snapshot opened before them still answers), and `the_segment_cap_is_reached_without_auto_merge_and_not_with_it` (the 256th refresh is refused without merging, all 300 succeed with it). The second was observed failing (40 segments after 40 refreshes) with the policy disabled before it passed. Node: the flag parses, defaults to 4 (16 when first written), `0` disables, over 1,024 is refused (`cli` test); the node passes its setting to every index schema and the engine clamps a larger value to 4,096 (`node` unit test). Not covered by a test: publish and reopen after merges, and remote segments.

## Follow-up: a smaller tier is better, so the default is now 4

The merge still ran inline, so the worst call was set by the biggest merge a tier allows. A sweep on a faster, noisier machine (same in-memory harness, relative numbers only, one run per cell unless noted; runs of the same cell differed by up to 2x in p50, so only the worst call and the bytes are read):

| batch x calls | `auto_merge_mb` | worst call | p99 | bytes written |
|---|---|---|---|---|
| 100 x 3,000 | 4 | 80 ms (second run 104) | 27 ms (40) | 117 MiB |
| 100 x 3,000 | 8 | 163 ms | 39 ms | 128 MiB |
| 100 x 3,000 | 16 | 249 ms (second run 266) | 36 ms (34) | 138 MiB |
| 1,000 x 800 | 4 | 136 ms | 110 ms | 138 MiB |
| 1,000 x 800 | 16 | 303 ms | 189 ms | 191 MiB |
| 4,000 x 240 | 2 | 304 ms | 229 ms | 141 MiB |
| 4,000 x 240 | 4 | 331 ms | 277 ms | 167 MiB |
| 4,000 x 240 | 16 | 405 ms | 373 ms | 234 MiB |

Worst call, tier 4 against 16: 3.1x smaller (second run 2.6x) at 100 x 3,000, 2.2x at 1,000 x 800, and only 1.2x at 4,000 x 240 (tier 2: 1.3x), which is inside the 2x run-to-run noise above, so no gain is shown at 4,000 per batch. Bytes written, tier 4 against 16: 15%, 28% and 29% fewer. Against off the default of 4 was not measured here, so its extra bytes over the old behaviour are unknown (at 300,000 small documents tier 4 wrote 117 MiB against 138 at 16 and 116 off in the first sweep, a different machine, so that comparison is indicative only).

Making each merge narrower (`max_merge_at_once` and `segments_per_tier` set together; 100 per batch, one run each; the table lists worst call and bytes) was tried and not adopted:

| calls | `auto_merge_mb` | width (at once / per tier) | worst call | bytes written |
|---|---|---|---|---|
| 1,500 | 16 | 10 / 8 | 155 ms | 64 MiB |
| 1,500 | 16 | 4 / 4 | 129 ms | 79 MiB |
| 1,500 | 16 | 3 / 3 | 140 ms | 80 MiB |
| 1,500 | 16 | 2 / 2 | 170 ms | 98 MiB |
| 3,000 | 4 | 10 / 8 | 80 ms | 117 MiB |
| 3,000 | 4 | 4 / 4 | 84 ms | 145 MiB |
| 3,000 | 8 | 10 / 8 | 163 ms | 128 MiB |
| 3,000 | 8 | 4 / 4 | 180 ms | 155 MiB |
| 3,000 | 16 | 10 / 8 | 249 ms | 138 MiB |
| 3,000 | 16 | 4 / 4 | 372 ms | 172 MiB |

Narrower merges never cut the worst call meaningfully and always wrote more bytes (up to 25% more at 3,000 calls, 53% at 1,500 for 2 / 2).

Downsides that remain: the policy only merges segments below half the cap, so at 4 a refresh that writes 2 MiB or more per segment is never merged by the policy and the forced whole-index merge at the segment cap comes back for that workload (4,000-document batches are in that range; the 4,000 x 240 cell makes only 240 calls, below the 256-segment cap, so it cannot show that fallback and its 331 ms is a pre-cap figure). The merge is still inline under the engine's lock, so this bounds the stall rather than removing it. Memory and I/O per merge should fall with a smaller tier but were not measured. Not measured on RustFS (the harness uses the in-memory store); the background merger follow-up will measure there.

## Finding: a first swap to lucene-rust's concurrent writer (not merged)

Tried while looking for a background merger (task M21.10). `ConcurrentIndexWriter` (pinned rev 17a6254) already merges segments flushed by refreshes without a commit (its test `refreshes_without_commits_are_merged_by_the_scheduler`) and runs with a borrowed directory through `run_merges(&stop)`. It has no path for hsearch's rich `document::Document`: its adds take stored-field or pre-inverted explicit documents, while hsearch calls `IndexWriter::update_fields_documents`, whose inversion needs the single writer's `&mut self` state. The adapter was switched to `ConcurrentIndexWriter` with updates routed through `with_writer`; the engine tests then failed: `auto_merge_keeps_segments_few_and_an_open_snapshot_readable`, `without_auto_merge_every_refresh_adds_a_segment`, `the_segment_cap_is_reached_without_auto_merge_and_not_with_it` and `query::tests::phrase_lowering_preserves_explicit_position_holes`, each finding 0 documents after a refresh, because the concurrent flush and snapshot do not see documents buffered in the inner writer. The attempt was a local stash, not committed or pushed, so it is not reproducible from the repository; M21.10's first step is to redo it against the upstream change. Tests that poison the old native lock on a panic also need the engine to keep that fact itself, since the concurrent writer recovers its own locks.
