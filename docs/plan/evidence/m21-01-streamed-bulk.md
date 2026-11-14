# M21.01 evidence: streamed bulk bodies, document limits, memory gate

Provisional: 4-CPU cloud container, in-memory object store, one node, Linux. Relative numbers only (M02.05 rule: RSS is measured, not summed; allocator-dependent).

## What changed

- `_bulk` is read as a stream (`crates/node/src/bulk.rs`, `Stream`): one action and its source at a time through a reused line buffer of at most `buffer_bytes`. A line longer than allowed is counted and discarded as it streams past; the stream then resynchronises on the next line.
- Items are written in batches that end at 1,000 items or half the buffer, so the pending batch is bounded by the buffer, not by the body.
- Limits (`crates/node/src/limits.rs`, defaults are the M02.05 values, flags on the binary): document 16 MiB (`--max-document-bytes`, **hard ceiling 200 MiB, refused at configuration**), bulk body 64 MiB (was a 10 MiB example constant), 5,000 items, parse buffer 8 MiB per request, 256 MiB reserved across concurrent bulks.
- A document over the limit fails alone with 413 `document_too_large_exception`, naming the limit, before anything of it reaches the write path. A document over the buffer but under the limit is refused naming the buffer.
- Memory gate: a bulk reserves `3 x min(body, buffer) + 64 KiB` before reading; if the node's budget is spent the request is refused with 429 `es_rejected_execution_exception` instead of queueing unread bytes. Small bodies cost little, so small bulks stay concurrent.
- Ingest moves strings from the parsed value into the operation instead of cloning them (`typed_owned`): one fewer copy of every text field.

## Measured

`crates/node/tests/memory.rs` (buffer 1 MiB, process RSS high-water mark above the pre-request level, whole process: server threads and the streaming test client). Eight consecutive runs:

| Body | Peak RSS above baseline |
|---|---|
| 30 documents of 700 KiB (21 MiB body) | 5.7-7.1 MB |
| 300 documents of 700 KiB (210 MiB body) | 4.8-8.5 MB |
| one 200 MiB document | 0-0.9 MB |

Ten times the body costs no more memory; peak stays within about 8 buffers. Nothing in these bodies is retained (the engine refuses fields over 8 KiB and the node refuses documents over the buffer), so the growth is request memory.

Throughput, `cargo run --release -p hsearch-node --example bulk_profile -- 200000 5000`, three runs each, same host: before 21,710 / 21,778 / 24,931 docs/s; after 21,492 / 21,587 / 21,506 docs/s. No regression beyond the spread seen before the change (the after runs sit at the low end of it). Peak RSS of that run is about 274 MB and is the engine and store holding 200k committed documents, not request memory.

## Trade-offs and downsides

- **Partial apply on a broken stream.** The old parser validated the whole body before committing anything. A stream cannot: a malformed action line after the first batch leaves the earlier batches written. The request is still refused with 400 and the reason says how many items were already applied; a body that breaks inside its first batch commits nothing, as before. Bulk is not atomic in OpenSearch either, but it does parse the whole body first.
- **The engine cannot ingest a large document today.** `hsearch-engine` caps a field and a document at 8 KiB and the slot format caps a field at 8 KiB (`MAX_DOCUMENT_BYTES`, `MAX_FIELD_BYTES`, Scale profile), so a document the node accepts under the buffer is still refused per item by the write path. The 200 MiB document is therefore proved refused with bounded memory, not ingested. Raising those caps belongs to the format and engine tasks, not to this change; no Lucene-side memory that grows with document size was found, because no document that large reaches Lucene.
- **Effective document ceiling is the smaller of the document limit and the buffer** (8 MiB by default). Documents between the two are refused with a reason naming the buffer. Admitting them needs field values streamed into the engine rather than materialised.
- **Limit is per profile, not per index.** Per-index settings storage arrives with the canonical settings form (M21.02 onward); the node refuses `settings` in a create body today.
- **429 not queueing.** A burst of large bulks is refused rather than delayed; clients must retry. A queue would hold sockets and workers, which have no read timeout (known gap from M20.04: stalled connections occupy workers).
- **Bulk body default raised from 10 MiB to 64 MiB** to the M02.05 frozen value. Safe now because memory no longer follows the body; the byte limit is still enforced as bytes are read, and a declared Content-Length over it is refused unread.
