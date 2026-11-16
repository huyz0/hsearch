# RustFS measurements of the write, visibility and large-document paths

Provisional: digest-pinned RustFS 1.0.4 (`scripts/rustfs.sh up`) in a container on a shared 4-CPU cloud host, loopback network, single disk, release build, one node. Relative numbers only; loopback hides network latency, so every latency here is a lower bound for a remote store. Reproduce:

    eval "$(scripts/rustfs.sh up)"
    cargo run --release -p hsearch-node --example rustfs_profile -- visibility 100 | cold | delete | large 200

This answers the "not measured on a real store" items left by M03.04 (modelled only), M21.09 increment 4 (`remove_version` never run against RustFS) and the unmeasured 200 MiB document. There is no AWS S3 here, so RustFS stands in for both a MinIO and an S3 run; S3 itself (cross-AZ latency, request throttling, strong list semantics) is still unmeasured.

## Visibility after a write (target: under 500 ms p50, under 5 s p99)

100 trials per row, requests counted by the S3 adapter. A durable write is not searchable until a refresh (timed refresh is M06.07), so "write to searchable" is the durable ack plus one explicit refresh.

| batch | durable ack p50 / p99 | write to searchable p50 / p99 | `Wait::Searchable` p50 / p99 (max) | PUTs per bulk |
|---|---|---|---|---|
| 1 doc | 5.0 / 6.5 ms | 13.8 / 23.2 ms | 13.2 / 24.4 ms (179 ms) | 1 durable, 1 searchable |
| 100 docs | 6.4 / 8.3 ms | 20.6 / 31.5 ms | 19.6 / 271 ms (1,377 ms) | 1, 1 |

Both targets are met with large margin on loopback. Visibility adds no GET, PUT, HEAD or LIST (counts identical for durable and searchable). The tail is the only concern: one `Wait::Searchable` call of a 100-document batch took 1.4 s, at least 2 of 100 above 250 ms; the cause was not isolated (not a store request, since PUTs per bulk stay 1) and is the first thing to profile if the p99 target tightens. Downside of the refresh model: a durable-only write is invisible until someone refreshes.

## Requests (every phase: LIST 0)

- Durable or searchable bulk: 1 PUT, 0 GET, 0 HEAD.
- Cold node on an index of 10,000 documents (new node, empty directory): 103 GET + 104 HEAD + 1 PUT, 407 ms to the first count. The PUT is the node's own write on open.
- Warm searches (term, range, match, 50 each): 0 requests, p50 0.3 to 0.4 ms, p99 0.5 to 0.7 ms.

## `remove_version` on RustFS (M21.09 gap closed)

Now a test: `crates/s3/tests/rustfs.rs::removing_a_version_deletes_only_it_and_is_repeatable`. Observed: one DELETE and nothing else per call (21 ms for an 8 MiB object); the removed version reads `NotFound`, its key heads `NotFound`, a neighbour object is untouched, a repeat removal answers `Ok` (RustFS, like S3, reports success for a missing version, as the trait documents), removing a version id against another key also answers `Ok`, and the key accepts a create again. Callers must therefore not read `Ok` as "something was deleted".

## 200 MiB document (M21.09)

One document, text field of 200 MiB less 1 KiB of JSON keys (the ceiling counts the whole document: a bare 200 MiB field is refused 413 `document_too_large_exception`, as intended), two runs.

| step | run 1 | run 2 |
|---|---|---|
| durable ack | 2.61 s (76.7 MiB/s), 26 PUT | 2.75 s (72.9 MiB/s), 26 PUT |
| plus refresh, searchable | 7.98 s | 7.45 s |
| peak RSS over baseline | 1,084 MiB (5.4x) | 1,084 MiB (5.4x) |
| replay by a new node, empty directory | 6.64 s, 29 GET + 30 HEAD + 1 PUT | 6.24 s, same |

26 PUTs are 25 chunks of 8 MiB plus the slot. 1.08 GiB of working memory is 5.4x the document, under the node's 32x admission charge (6.25 GiB), and below the 8 to 9x measured on the in-memory double (that double keeps its own copy). Smaller sizes: 16 MiB 0.26 s durable, 66.7 MiB RSS (4.2x); 64 MiB 0.81 s durable, 373 MiB (5.8x), 2.1 s replay. Replay memory was not measured. Downsides: replay is sequential reads of every chunk (HEAD per chunk beside each GET); a 200 MiB document is 8 s from write to searchable and 6.5 s to rebuild after a restart, so recovery time scales with document size. Crash orphans are still uncollected (a bucket expiry rule is the open fix, separate work).

## Page cache options on RustFS (PR 87's `cache_bench`)

Run for PR 87's thread, head d2c0e65, 200,000 documents, corpus 332.9 MiB, 60 queries, one process per mode:

| mode | open | GETs / MiB (total) | 60 queries | peak RSS |
|---|---|---|---|---|
| legacy (before) | 173 ms | 463 / 144.9 | 265 ms | 692 MiB |
| settings default | 183 ms | 531 / 144.9 | 308 ms | 673 MiB |
| spill, no demand paging | 568 ms | 321 / 332.9 | 7.2 ms | 593 MiB |
| heap, no demand paging | 709 ms | 321 / 332.9 | 11.1 ms | 802 MiB |
| fixed profile 20 ms, 100 MB/s | 305 ms | 413 / 236.9 | 160 ms | 618 MiB |

On loopback the fixed profile fetches 63% more bytes than the default but 22% fewer GETs and halves query time; its cost is bytes, which are free here and not at 20 ms latency. The default's 15% more GETs than legacy is from one run each and unconfirmed.
