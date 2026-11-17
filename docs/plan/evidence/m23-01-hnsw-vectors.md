# M23.01 evidence: HNSW vectors through the engine adapter

Provisional: every number below is from one shared 4 vCPU x86 cloud VM, release build, in-memory directory, one thread, other jobs possibly running. They are relative, not absolute, and there is no ARM run. The corpus is synthetic (see below). No standard dataset (SIFT1M, GIST1M) was reachable from this environment, so the "stated corpus" requirement is met only for the synthetic one and the real-dataset rerun is **unrun**.

## What exists

`hsearch-engine` takes a `Kind::Vector(VectorSpec)` field and a `Query::Knn`:

- `VectorSpec { dimension (1..=4096), similarity (L2, cosine, dot product, max inner product), m (1..=512), ef_construction (1..=3200), quantizer }`. A schema holds at most 4 vector fields. A vector is required on every document like any other field, is indexed and is **not stored**: a projection or sort on it is refused (`Unsupported`), and its values are recovered from the segment's raw vectors when a checkpoint is verified.
- Quantizers: none, Lucene scalar (8, 7, 4, 2, 1 bits), BBQ, RaBitQ and extended RaBitQ (2 to 8 bits). Each keeps the raw vectors and the same HNSW graph; the codes are added beside them. These are the OpenSearch mapping parameters M02.12 chose (`m`, `ef_construction`, `space_type`, `compression_level` and the `extended` engine's quantizers); faiss and the trained-model path stay out.
- `Query::Knn { field, vector, k, candidates, filter }`. The filter is applied inside the graph walk, so a 5% filter still returns `k` hits (measured below). The result composes in a `Bool` as fixed documents with fixed scores (Lucene's own rewrite), so it can sit in `must`, `filter`, `should` or `must_not`. `k` is bounded by 10,000 and `candidates` (the `ef_search` beam) by 10,000; a query vector of the wrong length or with a non-finite component is refused.
- New capability `QueryKnn` in the engine inventory.

The engine imports one more upstream crate, `lucene-util`, for `ScalarEncoding` (allowed in `tools/workspace.py`; same pinned revision `df02d694`).

## Corpus and method

`crates/engine/tests/vector.rs` and `crates/engine/examples/vector_bench.rs` generate vectors around 16 (tests) or 64 (bench) Gaussian cluster centres with unit-variance noise. This is tight, isotropic clusters, which is the **hard case for 1-bit codes**. Recall@10 is measured against an exact scan of the same corpus. The bench is one process per variant:

    cargo run --release -p hsearch-engine --example vector_bench -- [variant] [N] [DIM] [BEAM]

## Recall floors (test, 3,000 vectors, 32 d, L2, 40 queries)

The floors M02.12 froze are 0.95 for float HNSW and 0.90 for quantized variants. All pass in `every_variant_meets_its_recall_floor`; the 1-bit codes pass only at a wider beam.

| Variant | Beam | recall@10 | Floor |
| --- | --- | --- | --- |
| plain | 100 | 0.98 | 0.95 |
| scalar 8 / 7 / 4 bit | 100 | 0.98 / 0.98 / 0.975 | 0.90 |
| scalar 2 bit | 100 | 0.92 | 0.90 |
| extended RaBitQ 4 bit | 100 | 0.9775 | 0.90 |
| RaBitQ | 200 | 0.9475 | 0.90 (0.85 at beam 100) |
| scalar 1 bit, BBQ | 400 | 0.96, 0.965 | 0.90 (0.65 at beam 100; 0.84 at 200) |

Other checks, all in `tests/vector.rs`: a 5%-selectivity filter returns exactly `k` hits with recall 1.0 against the filtered exact scan; a filter matching fewer than `k` documents returns only those; a third of the corpus deleted and one vector replaced over three segments keeps recall 1.0 and never returns a deleted document, before and after `merge()`; a committed index reopened with its checkpoint (which re-reads every stored vector against the retry digests) and opened read-only with `Snapshot::open_committed` returns the same top-10 for plain, 4-bit scalar and BBQ. Seen red: the filter tests fail when the filter is not passed to the search.

## Cost, latency and size (bench, 50,000 vectors, 128 d, L2, 200 queries)

| Variant | Beam | Build | Index | Peak RSS over baseline | Query p50 / p99 | recall@10 |
| --- | --- | --- | --- | --- | --- | --- |
| plain | 100 | 5.2 s | 26.5 MiB (555 B/vector) | 91 MiB | 1.04 / 1.47 ms | 0.9035 |
| plain | 200 | | | 91 MiB | 1.34 / 3.19 ms | 0.9545 |
| plain | 400 | | | 92 MiB | 1.66 / 2.03 ms | 0.9890 |
| scalar 8 bit | 100 | 5.5 s | 33.4 MiB (699 B) | 103 MiB | 0.93 / 1.93 ms | 0.9045 |
| scalar 4 bit | 100 | 5.5 s | 30.3 MiB (635 B) | 95 MiB | 0.84 / 1.24 ms | 0.8985 |
| scalar 4 bit | 200 | | | 96 MiB | 1.16 / 1.90 ms | 0.9565 |
| extended RaBitQ 4 bit | 100 | 5.4 s | 30.1 MiB (631 B) | 100 MiB | 1.21 / 1.67 ms | 0.9090 |
| extended RaBitQ 4 bit | 400 | | | 100 MiB | 1.96 / 3.59 ms | 0.9895 |
| BBQ | 100 | 5.2 s | 28.0 MiB (587 B) | 93 MiB | 0.89 / 1.43 ms | 0.683 |
| BBQ | 400 | | | 93 MiB | 1.72 / 2.63 ms | 0.8645 |
| RaBitQ | 400 | | | 93 MiB | 1.74 / 2.43 ms | 0.8645 |

## Trade-offs and downsides

- **Quantization does not shrink the index here.** Every quantized variant keeps the raw float vectors (the rerank and the merge need them) and adds codes, so the index is 13% to 26% **larger** than plain. The gain is in what a search touches: the graph walk scores small codes, which matters when the vectors do not fit in memory (M23.03), not for an in-memory directory, where latency is within noise of plain at equal beam. This is why quantization is a per-field setting, off by default (non-negotiable on settings with a measured downside).
- **1-bit codes (BBQ, RaBitQ, scalar 1 bit) lose recall on this corpus** and plateau near 0.86 at beam 400 at 128 d, where 4-bit codes reach 0.99. They are the setting for data whose neighbours are well separated; they need a wide beam, and the ledger does not claim a floor for them on a real dataset. The 4-bit codes are the safe quantized choice.
- **Recall at beam 100 depends on corpus size.** Plain HNSW reaches 0.98 at 3,000 vectors but 0.90 at 50,000; the 0.95 floor needs beam 200 at that size. `candidates` is a per-query knob and costs latency roughly linearly (1.04 to 1.66 ms p50 from beam 100 to 400).
- Build time is about the same for every variant (5.2 to 5.5 s for 50,000 vectors); peak memory adds 0 to 12 MiB for the codes.
- A kNN search is resolved before the rest of the query runs, so a `Bool` filter next to a kNN removes hits from the top `k` rather than widening it. Put the restriction in the kNN's own `filter` to get `k` filtered hits.
- Aggregations over a kNN-scoped set are not supported yet (`Unsupported`).
- The vector is in the document and so in the retry digest: an exact retry is recognised by bit-identical components.

## Upstream vector tests rerun at the pin

Run in a clone of lucene-rust at `df02d694` (the revision hsearch builds), release profile, filters as in M02.12; no failure in any:

| Command | Passed |
| --- | --- |
| `cargo test --release -p lucene-codecs hnsw` / `knn` / `quantiz` | 105 / 15 / 29 |
| `cargo test --release -p lucene-search -- knn hnsw vector` | 77 and 15 more across the filters |
| `cargo test --release -p lucene-index -- hnsw vector` | 102, 3, 1 |
| `-p lucene-vector-quant` | 54 (1 ignored microbench) |
| `-p lucene-disk-hnsw` | 13 + 13 |
| `-p lucene-ivf` | 41, 3, 11, 3, 2, 26, 5 |

## Not done here

- Concurrent-segment search versus sequential at hsearch segment sizes: unrun (M02.12 saw concurrent slower at 4,000 documents).
- Real-dataset recall (SIFT1M, GIST1M), ARM, and concurrent-merge timing at 100k to 1M vectors per segment: unrun.
- REST `knn_vector` mapping parameters, the `knn` query DSL, and the vector field in the durable op log are not wired: the log's `Field` carries no vector, so the engine feature is not yet reachable from the HTTP node.
