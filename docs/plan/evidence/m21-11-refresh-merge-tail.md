# The 1.37 s refresh stall: cause, fix, numbers

Provisional: 4-CPU cloud container, in-memory object store (no network), one node, release build, local scratch disk. Relative numbers only. Reproduce: `cargo run --release -p hsearch-node --example visibility_tail -- 3000 100 250 <auto_merge_mb>` (0 is the old behaviour).

## What was seen

[m21-10](m21-10-rustfs-measurements.md) found one `Wait::Searchable` call of a 100-document batch taking 1.37 s on RustFS, in two runs, against a 20 ms median.

## Cause

It does not need a network: it reproduces on the in-memory store, at the same call index every run. The engine's writer had no merge policy, so every refresh added one segment and nothing merged them. At 254 refreshes the segment cap (256) was reached, the write was refused with `Limit`, and the index writer committed and ran `force_merge(1)`, one blocking merge of the entire index, on that write. It recurs every 254 refreshes and grows with the index: the stalled call took 1.4 s with 25,000 documents, 2.3 s with 280,000 and 3.2 s with 509,000 (batches of 1,000), so the visibility p99 target is lost at a few hundred thousand documents on any steady refresh load, not only once.

Because the cost is per call index, not per document, the same stall shows with batches of 10, 50 and 100 (documents before the stall scale with the batch).

## Fix

An engine setting `auto_merge_mb` (`Limits`, node flag `--auto-merge-mb`, default 16, 0 turns it off): the writer gets a tiered merge policy whose largest merged segment is that many MiB. The policy runs inside the refresh's flush and folds small segments as they appear, so segments stay few and no refresh merges more than a few small ones. Larger segments are left alone; the forced merge at the cap stays as the backstop and is now rarely reached. The engine's own default stays 0 (the research profile and every existing test pin the old behaviour); the node turns it on.

## Numbers

3,000 `Wait::Searchable` bulks of 100 documents (300,000 documents), 3 runs each:

| | worst call | p50 | p99 | whole run | process bytes written | search p50 / p99 after |
|---|---|---|---|---|---|---|
| off (old) | 2.25 to 2.36 s, call 2795 | 14.8 to 17.7 ms | 29 to 38 ms | 65 to 76 s | 116 MiB | 2.5 to 3.9 / 3.9 to 8.5 ms |
| 16 MiB | 0.26 to 0.31 s | 9.1 to 10.8 ms | 35 to 37 ms | 31 to 38 s | 138 MiB | 2.0 / 2.8 to 4.2 ms |

At 25,000 documents (the m21-10 size) the worst call went from 1.4 to 1.7 s to 0.12 to 0.13 s. A short sweep of 16, 64 and 256 MiB at 80,000 documents gave the same worst call, about 0.12 s; at 300,000 documents 64 MiB left a 0.38 to 0.40 s stall against 0.22 to 0.31 s at 16 MiB, so the default is 16.

## Trade-offs and downsides

- The worst call fell about 8x and the median and whole-run time roughly halved (fewer segments to flush against and search), but p99 did not improve (about 36 ms, against 29 to 38 ms off): the merges move cost from one huge stall to small stalls spread over many calls.
- Bytes written rose 19% (138 against 116 MiB for 300,000 documents; the process counter includes the in-memory store's copies, so this is a ratio, not an absolute). With an object store, merged segments are uploaded again at publish, so the PUT bytes rise by a similar share. PUT counts per bulk are unchanged (merging is local); the publish path was not measured against RustFS.
- A 0.26 to 0.31 s stall remains where a larger tier merges (about every 1,800 refreshes here). It is bounded by the 16 MiB cap, not by the index.
- The merge runs inline on the write path. A background merge would hide it, but needs a scheduler and a shared lock with refresh; not done.
- Not measured: remote (range-read) segments, concurrent writers, deletes and updates (the merge also reclaims deleted copies), memory during a merge.
- There is no per-index setting yet: only the node-wide flag. The setting is part of the engine schema's equality, so a checkpoint reopens under the value it was taken with; within one node process the flag is constant.

## Tests

`without_auto_merge_every_refresh_adds_a_segment` (40 refreshes, 40 segments, all documents found: pins the old behaviour) and `auto_merge_keeps_segments_few_and_an_open_snapshot_readable` (40 refreshes, at most 20 segments, a snapshot opened before the merges still answers). The second was observed failing (40 segments after 40 refreshes) with the policy line disabled before it passed. Node: the flag parses, defaults to 16, `0` disables, over 1,024 is refused.
