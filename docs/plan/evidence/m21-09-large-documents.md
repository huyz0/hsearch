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

A string field over 1 MiB is no longer written into the log slot. The writer first creates its text as create-only chunk objects of at most 8 MiB under `<index>/blob/<epoch>/<attempt>/<seq>/<op>.<field>/<chunk>`, then writes the slot, which holds a 29-byte reference (epoch, attempt, chunk count, length). The slot is written last, so it is what makes the chunks reachable. Replay reads each chunk back, checks the chunk count and total length against the reference (a mismatch is Corrupt) and rebuilds the string. The document ceiling is now the product's 200 MiB (`MAX_INDEXED_DOCUMENT_BYTES`); the node's cap is the smaller of its configured limit and its buffer. Tests: `a_document_over_the_inline_limit_goes_to_chunk_objects_and_replays` (12 MiB, slot under 1 KiB, rebuilt from the store alone), `a_large_string_is_replaced_by_a_reference_and_resolved_on_decode`, and the node's 10 MiB end-to-end test.

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
- The attempt number is unique per write attempt, so chunks left by a refused slot write never collide with the retry at the same position (found in review).
- Replay checks each chunk's length against the reference before allocating it. Increment 3 below bounds the text of one slot.
- A crash after the chunks and before the slot leaves orphan chunk objects. They are never read, and nothing collects them yet.
- Chunks go through the ordinary create path, not the multipart `LargeObjectWriter` seam (M03.14, still planned), so each chunk is bounded by the 16 MiB payload limit.

# M21.09 evidence (increment 3): 100 MiB measured, replay bounded

Provisional: same environment. The in-memory store is capped at 64 MiB by design, so `large_field_profile` now runs on a local double (`BigStore`, in the example only) that holds up to 200 MiB. It is not conformance-tested and not a real object store; the numbers measure the node, index and engine, with the store's own copy of the document included.

| document | time | peak over baseline |
|---|---|---|
| 4 MiB | 0.14 s | 36.4 MiB (9.1x) |
| 32 MiB | 1.25 s | 252.9 MiB (7.9x) |
| 64 MiB | 2.38 s | 550.7 MiB (8.6x) |
| 100 MiB | 3.40 s | 844.8 MiB (8.4x) |

One run each, wait for searchable. Memory stays near 8-9x the document across the range, under the 32x admission charge (3.2 GiB for 100 MiB), so the charge is unchanged and conservative. Throughput is 25-29 MiB/s throughout. A 100 MiB document therefore needs about 850 MiB of working memory. The `BigStore` double keeps every version and a hash of every request, so the ratios are upper bounds for the node and index. The 200 MiB ceiling itself was not measured.

## Replay bound

A slot may carry at most `slot_text_cap(document_bytes)` bytes of string text in total (`text_len`, externalised strings counted whole): the larger of the 8 MiB slot bound and the schema's document limit, so at most 200 MiB in the node. A bulk whose own text exceeds it is refused `Invalid` (400, permanent, checked alone); a bulk that only fails to fit beside others in the same group is refused `Capacity` (retryable), as for the existing byte bound. Tests: `a_bulk_whose_text_can_never_fit_one_slot_is_invalid_not_retryable`, and `slot_text_cap`/`text_fits` boundary assertions.

What this does and does not bound: it bounds the string text one newly written slot makes replay hold, to one document's ceiling. Peak memory to replay a full slot is still about 8-9x its text (the ratio measured above, which includes indexing), so up to roughly 1.7 GiB for a 200 MiB slot, and replay has no admission charge like the write path's. Replay still materialises a slot's strings together instead of streaming them operation by operation. Slots written before this rule are not re-checked on replay.

## Orphan chunks: not done, and why

Nothing in the object seam can delete an object (`ObjectBackend` has head, create, read_range, compare_exchange and resolve only), and the repo forbids LIST on write and startup paths, so a failed or crashed attempt's chunks can neither be deleted by the writer nor found by a sweep. Collecting them needs a decision first: either a delete (or lifecycle-expiry) capability on the seam with conformance tests, or a derivable record of attempts. That is a seam change across `hsearch-object`, the S3 store and the conformance suite, so it is left as its own task rather than forced into this change.

M21.09 stays open: orphan collection and a measured 200 MiB run remain.
