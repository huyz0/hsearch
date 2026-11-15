# M21.09 evidence (increment 1): retry history keeps a digest, not the document

Provisional: 4-CPU cloud container, in-memory object store, one node, Linux, release build. Relative numbers only. Reproduce with `cargo run --release -p hsearch-node --example large_field_profile -- <MiB> 1`.

## What changed

The engine kept an owned copy of every admitted document's field values in its retry history, only to recognise an exact retry (same request id, same bytes) by structural equality. `Operation` now holds the identity, version, kind and a 128-bit digest of the values. An exact retry has the same digest and replays its receipts; one changed byte under the same request id is a conflict (`retry_history_holds_a_digest_and_still_tells_a_retry_from_a_conflict`). The prepared batch also builds each `Document` straight from the borrowed mutation, so the owned copy is not made at all.

Downside: equality is now by a 128-bit non-cryptographic digest, not by content: two SipHash-1-3 passes under two independent random keys drawn once per process. The keys are unknown to anyone who can send a document, so a colliding payload cannot be built offline, and an accidental collision is a 2^-128 event; but it is not a proof of equality, and a collision under a reused request id would replay the earlier receipts instead of reporting a conflict. The keys do not survive a restart, which is fine because the history and checkpoints are in-process only. The document is still stored and indexed exactly as sent.

## Numbers

One document through `Node::bulk`, wait for searchable, peak RSS over baseline (3 runs each), before from `m21-08-large-text-fields.md`:

| document | before | after |
|---|---|---|
| 1 MiB | 15.8-16.1 MiB (about 16x) | 12.6-12.8 MiB (about 12.7x) |
| 3.9 MiB | 49.0-49.2 MiB (12.6x) | 38.1-38.3 MiB (9.8x) |

Throughput is unchanged (25-29 MiB/s). The node's admission charge stays at 32x, which still covers every measured size, so it is not lowered on one run.

# M21.09 evidence (increment 2): large strings as their own objects

Provisional: same environment as above. Reproduce with `large_field_profile -- <MiB> 1`.

## What changed

A string field over 1 MiB is no longer written into the log slot. The writer first creates its text as create-only chunk objects of at most 8 MiB under `<index>/blob/<epoch>/<seq>/<op>.<field>/<chunk>`, then writes the slot, which holds a 21-byte reference (epoch, chunk count, length). The slot is written last, so it is what makes the chunks reachable. Replay reads each chunk back, checks the chunk count and total length against the reference (a mismatch is Corrupt) and rebuilds the string. The document ceiling is now the product's 200 MiB (`MAX_INDEXED_DOCUMENT_BYTES`); the node's cap is the smaller of its configured limit and its buffer. Tests: `a_document_over_the_inline_limit_goes_to_chunk_objects_and_replays` (12 MiB, slot under 1 KiB, rebuilt from the store alone), `a_large_string_is_replaced_by_a_reference_and_resolved_on_decode`, and the node's 10 MiB end-to-end test.

## Numbers

One document through `Node::bulk`, wait for searchable, peak RSS over baseline (one run each):

| document | peak over baseline |
|---|---|
| 4 MiB | 40.6 MiB (10.2x) |
| 8 MiB | 74.1 MiB (9.3x) |
| 12 MiB | 119.2 MiB (9.9x) |

About 10x the document, under the 32x admission charge, so the charge is unchanged.

## Downsides and what is not done

- Chunks are written one after another, so a very large document costs one PUT per 8 MiB before the slot PUT.
- A crash after the chunks and before the slot leaves orphan chunk objects. They are never read, and nothing collects them yet.
- Chunks go through the ordinary create path, not the multipart `LargeObjectWriter` seam (M03.14, still planned), so each chunk is bounded by the 16 MiB payload limit.
- Measured only to 12 MiB. The 100 MiB end-to-end run and the 100 MiB memory curve are not done, so M21.09 stays open.
