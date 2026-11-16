# RustFS measurements of the write, visibility and large-document paths

Provisional: digest-pinned RustFS 1.0.4 (`scripts/rustfs.sh up`) in a container on a shared 4-CPU cloud host, loopback network, single disk, release build, one node. Relative numbers only; loopback hides network latency, so every latency here is a lower bound for a remote store. Every row is one run of the example unless a count says otherwise, and each run leaves its `hsp-<nanos>` bucket in RustFS (about 2x the document for `large`); remove it with `scripts/rustfs.sh down`. Percentiles at 100 trials are the 2nd-largest sample, and the index grows from 0 to about 20,000 documents during `visibility` (the 100-document rows run on an index already holding the 1-document rows' data). Reproduce:

    eval "$(scripts/rustfs.sh up)"
    cargo run --release -p hsearch-node --example rustfs_profile -- visibility 100 | cold | delete | large 200

This answers the "not measured on a real store" items left by M03.04 (modelled only), M21.09 increment 4 (`remove_version` never run against RustFS) and the unmeasured 200 MiB document. There is no AWS S3 here, so RustFS stands in for both a MinIO and an S3 run; S3 itself (cross-AZ latency, request throttling, strong list semantics) is still unmeasured.

## Visibility after a write (target: under 500 ms p50, under 5 s p99)

100 trials per row, requests counted by the S3 adapter, per bulk and per refresh. Run 2 of the harness (after review fixes) is quoted; the first run was within 10% except the tails noted. A durable write is not searchable until a refresh (timed refresh is M06.07), so "write to searchable" is the durable ack plus one explicit refresh.

| batch | durable ack p50 / p99 | write to searchable p50 / p99 | `Wait::Searchable` p50 / p99 (max) | PUTs per bulk |
|---|---|---|---|---|
| 1 doc | 5.6 / 9.4 ms | 14.1 / 19.2 ms | 14.6 / 19.1 ms (19.8 ms) | 1 durable, 1 searchable |
| 100 docs | 6.2 / 10.8 ms | 19.6 / 29.6 ms | 20.5 / 32.3 ms (1,370 ms) | 1, 1 |

Both targets are met with large margin on loopback. Over all 200 bulks and 200 refreshes of each batch size, the durable bulk, the refresh and the `Wait::Searchable` bulk issued 0 GET, 0 HEAD and 0 LIST, and the refresh 0 PUT; each bulk is exactly 1 PUT. The tail is the only concern: one `Wait::Searchable` call of a 100-document batch took 1.37 s in both harness runs (the first run also had a second slow call, 271 ms at p99), so it recurs; the cause was not isolated (it is not an extra store request, since PUTs per bulk stay 1) and is the first thing to profile if the p99 target tightens. Downside of the refresh model: a durable-only write is invisible until someone refreshes.

## Requests (every phase: LIST 0)

- Durable or searchable bulk: 1 PUT, 0 GET, 0 HEAD.
- Cold node on an index of 10,000 documents (new node, empty directory), two runs: 103 GET + 104 HEAD + 1 PUT, 335 to 407 ms to the first count. The PUT is a write the node issues on open; its object was not identified.
- Warm searches (term, range, match, 50 each): 0 requests, p50 0.3 to 0.4 ms, p99 0.5 to 0.7 ms.

## `remove_version` on RustFS (M21.09 gap closed)

Now a test: `crates/s3/tests/rustfs.rs::removing_a_version_deletes_only_it_and_is_repeatable`. Observed: one DELETE and nothing else per call (21 ms for an 8 MiB object, from the example); the removed version reads `NotFound`, its key heads `NotFound`, another object, and another version of the same key, are untouched, a repeat removal answers `Ok` (RustFS, like S3, reports success for a missing version, as the trait documents), and the key accepts a create again. The example alone also showed that removing a version id against a different key answers `Ok`; that is not asserted by the test. Callers must therefore not read `Ok` as "something was deleted".

## 200 MiB document (M21.09)

One document, text field of 200 MiB less 1 KiB of JSON keys (the ceiling counts the whole document: in a first attempt a bare 200 MiB field was refused 413 `document_too_large_exception`), two runs.

| step | run 1 | run 2 |
|---|---|---|
| durable ack | 2.80 s (71.4 MiB/s), 26 PUT | 2.75 s (72.8 MiB/s), 26 PUT |
| plus refresh, searchable | 8.02 s | 7.79 s |
| peak RSS over baseline | 1,092 MiB (5.5x) | 1,092 MiB (5.5x) |
| replay by a new node, empty directory | 6.40 s, 29 GET + 30 HEAD + 1 PUT | 6.27 s, same |

PUT counts of 3, 9 and 26 at 16, 64 and 200 MiB match ceil(size / 8 MiB) chunks plus the slot. Peak memory is the process high-water mark reset (`clear_refs`) after the input strings exist, minus resident memory at that point, so it is the node's own footprint: 1.07 GiB, 5.5x the document, under the node's 32x admission charge (6.25 GiB). The in-memory double's 8 to 9x (increment 3) used a different baseline and a store that keeps its own copy, so the two are not comparable. Smaller sizes, one run each: 16 MiB 0.25 s durable, 72.5 MiB (4.5x), 0.48 s replay; 64 MiB 0.93 s durable, 373 MiB (5.8x), 2.1 s replay. Replay memory was not measured. Replay issued 29 GET and 30 HEAD for 25 chunks; the extra requests were not attributed. Downsides: a 200 MiB document is 8 s from write to searchable and 6.5 s to rebuild after a restart, so recovery time scales with document size. Crash orphans are still uncollected (a bucket expiry rule is the open fix, separate work).

## Page cache options on RustFS (PR 87's `cache_bench`)

Not produced by this PR's code: run on PR 87's branch for that thread (`cargo run --release -p hsearch-engine --example cache_bench -- <legacy|settings|nodemand|nodemand-heap|profile>` with the `HSEARCH_S3_*` variables from `scripts/rustfs.sh up`), head d2c0e65, 200,000 documents, corpus 332.9 MiB, 60 queries, one process per mode:

| mode | open | GETs / MiB (total) | 60 queries | peak RSS |
|---|---|---|---|---|
| legacy (before) | 173 ms | 463 / 144.9 | 265 ms | 692 MiB |
| settings default | 183 ms | 531 / 144.9 | 308 ms | 673 MiB |
| spill, no demand paging | 568 ms | 321 / 332.9 | 7.2 ms | 593 MiB |
| heap, no demand paging | 709 ms | 321 / 332.9 | 11.1 ms | 802 MiB |
| fixed profile 20 ms, 100 MB/s | 305 ms | 413 / 236.9 | 160 ms | 618 MiB |

On loopback the fixed profile fetches 63% more bytes than the default but 22% fewer GETs and halves query time; its cost is bytes, which are free here and not at 20 ms latency. The default's 15% more GETs than legacy is from one run each and unconfirmed.
