# M21.08 evidence: large text fields up to the index's document ceiling

Provisional: 4-CPU cloud container, in-memory object store, one node, Linux, release build. Relative numbers only. Reproduce with `cargo run --release -p hsearch-node --example large_field_profile -- <MiB> <docs>` and `cargo run --release -p hsearch-engine --example term_count_profile -- <streamed|materialized> <MiB>`.

## What OpenSearch does, and what this matches

OpenSearch puts no size limit on a `text` field. The practical cap is the request body (`http.max_content_length`, 100 MiB by default), so a document cannot pass about 100 MiB. A `keyword` term is capped at 32,766 bytes (Lucene's `IndexWriter.MAX_TERM_LENGTH`). hsearch now matches the keyword rule exactly and the text rule up to **a document ceiling of 4,190,208 bytes** (`hsearch_index::MAX_INDEXED_DOCUMENT_BYTES`), and refuses anything larger by name. Reaching 100 MiB needs a large-document lane (see M21.09); it is not in this change.

## What changed

- `Limits` gains `document_bytes` and `terms` (`crates/engine/src/schema.rs`). `Limits::BOUNDED` and `Limits::scale()` keep 8,192 bytes and 128 terms, so every existing engine test passes unchanged; `with_document_bytes(n)` raises the document limit and lets the term bound stop binding, because a token is at least one byte. A keyword over 32,766 bytes is refused whatever the document limit.
- The term count for the term bound streams (`Analyzer::try_for_each_token`) instead of collecting every token. With a term limit set below the document size, a 4 MiB text field's check peaked at **4.0-4.1 MiB instead of 53.9 MiB, and ran in 0.035 s instead of 0.085 s** (3 runs each, 726,061 tokens). With the limit at the document limit, as the node sets it, no analysis pass is spent counting at all.
- The prepared-batch byte bound and the writer's RAM buffer follow `document_bytes`; the research profile's values are unchanged.
- The index log slot refuses a request over half a slot (4 MiB) as before; the 8 KiB field cap in the slot decoder is now the slot size. Size checks use `encoded_len` and the operations are moved, not cloned, into the slot: two clones of a large document per request are gone.
- The node's engine schema uses the ceiling; the node's own `--max-document-bytes` still applies to the source line. A source line over the ceiling is refused with 413 `document_too_large_exception` and the message "this build can index documents of at most N bytes".
- The node charges the indexing working set of each document of 64 KiB or more (32 x its size, above the 29x measured at 0.25 MiB) against the node's memory budget for the batch. When the budget cannot cover it, the batch's items are refused with 429 `es_rejected_execution_exception` and nothing is written. Smaller documents are charged nothing extra, so existing behaviour is unchanged for them.

## Numbers

One document of N MiB through `Node::bulk`, wait for searchable (3 runs each):

| document | time | throughput | peak RSS over baseline |
|---|---|---|---|
| 0.25 MiB | 0.01-0.02 s | 16-20 MiB/s | 7.1-7.4 MiB (about 29x) |
| 1 MiB | 0.04 s | 25-28 MiB/s | 15.8-16.1 MiB (about 16x) |
| 2 MiB | 0.07 s | 27-29 MiB/s | 27.7-28.0 MiB (about 14x) |
| 3.9 MiB | 0.14-0.15 s | 26-29 MiB/s | 49.0-49.2 MiB (12.6x) |

Peak memory **grows with document size** here. It is bounded by admission (the budget charge above), not by the streaming buffer: Lucene keeps the stored field and the inverted postings of a document in memory until it is flushed, and the engine keeps an owned copy for retry detection. The in-memory store used here also holds the slot and segment copies, which an object store would not, so the factor is an upper bound for the process, not for the node's request memory.

Small documents, no regression (`bulk_profile`, 100,000 documents, batch 5,000, 3 runs each, before then after): 24,536-29,197 docs/s before, 26,615-28,134 docs/s after (noise on a shared host); peak RSS 174.4-174.6 MB before, 173.9-174.3 MB after.

## What this does not do

- Documents between 4 MiB and 100-200 MiB are refused by name. A lane that stores a large document in its own object and streams it is M21.09.
- (Superseded by M21.09 increment 1: the retry history now keeps a digest.) The engine kept an owned copy of each recent document for exact-retry detection, bounded by the history window (4 MiB in the scale profile, so at most about one maximal document). Recognising a retry by digest would drop it. Recorded in M21.09. What it keeps for every live identity is a version and a 128-bit digest of the values (checkpoint verification compares the digest), not the text.
- The writer's RAM buffer is still sized from the research profile's document bound, so small-document indexes do not hold more RAM than before.
- Highlighting is not implemented in hsearch, so OpenSearch's `index.highlight.max_analyzed_offset` has no counterpart here.

## Review

Independent review found that live identities kept every document's text (now a version and a 128-bit digest), that the writer RAM buffer grew with the document limit (now unchanged), that the admission charge sat below the measured 29x at 0.25 MiB (now 32x), that a single-document write was cloned (now moved), and that the id margin ignored numeric fields (now 512 bytes). All fixed in the PR; CI green.
