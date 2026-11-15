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

## Not yet done (M21.09 stays open)

Documents between 4 MiB and 100-200 MiB still need their own object (the log slot is capped at 4 MiB per request by the 16 MiB object payload bound) and a measured admission factor for that range.
