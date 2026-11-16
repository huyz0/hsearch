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

# M21.09 evidence (increment 4): orphan chunks after a failed write

## What changed

`MetadataStore` gains `remove_version(key, version)`: permanently remove one immutable version, `NotFound` if already gone on `MemoryStore` (S3 answers success for a missing version, so callers treat both as removed), default `Unsupported` so existing stores and doubles are unchanged. `MemoryStore` implements it (frees the version's bytes and the current pointer, keeps the accepted-request record, so a long-lived double still counts the identity against its limits and replaying the original request would report an object that is gone), and `S3Store` delegates to its existing `delete_version`. The writer records each chunk's version as it creates it, and removes them best-effort when the slot that would name them certainly does not exist: a refused chunk (the earlier ones go), a refused slot write, an encode failure, or a slot payload that cannot be built. It never removes on `Fenced` or `Unknown`, because an unknown slot write may have committed a slot that names them. A delete that fails or is unsupported leaves an orphan and nothing else, since no reader looks at a chunk without a slot.

Tests: `removing_a_version_frees_its_bytes_and_leaves_other_objects_alone`, `a_removed_object_frees_room_in_a_full_store`, `a_store_without_delete_says_unsupported` (object conformance); `a_refused_slot_write_removes_the_chunks_it_wrote_and_the_retry_succeeds` (12 MiB, 2 chunks removed, retry commits), `a_refused_second_chunk_removes_the_first`, `a_store_that_cannot_delete_leaves_the_orphans_and_stays_correct`, and `an_unknown_slot_write_never_removes_the_chunks_it_may_name` (slot committed, answer lost and unresolvable: zero removals, and a new process replays the document) (index). The removal tests also check that each removal succeeded on a `blob/` key, not only that one was attempted. The first draft of the mid-chunk test matched the wrong key and was observed failing before the key was corrected.

## Not done

- Orphans from a crash between the chunk writes and the slot write are still never collected: the writer is gone and nothing records the attempt, and LIST is forbidden here. They are bounded by the writes in flight at crash time. Collecting them needs a recorded attempt or a lifecycle rule on the bucket's `blob/` prefix; neither is built.
- `S3Store::remove_version` is the existing delete, not newly exercised against RustFS here (no endpoint in this environment); the S3 integration tests skip without one.
- A bucket lifecycle expiry for `<index>/blob/` is not configured or documented anywhere yet.
- Removal runs one delete at a time on the writer thread after a definite failure, so a throttled store makes that bulk's refusal slower.

M21.09 stays open: crash orphans and a measured 200 MiB run remain. (Crash orphans: increment 5, below.)

# M21.09 evidence (increment 5): chunks orphaned by a crash

## Decision: a recorded attempt, not a lifecycle rule

A bucket lifecycle rule on `<index>/blob/` cannot be used, and not for cost. Expiry is by age, and a committed slot's chunks look exactly like an orphan's: the log is never truncated (open replays every slot from 0, `start` in `writer.rs`), so committed chunks must live as long as the index. An age rule would delete live documents. Making the rule safe needs a tag on every chunk and a retag after commit (one more PUT per chunk, doubling the large-field PUT cost), and the adapter has no tagging.

Measured on the pinned RustFS 1.0.4 (provisional: local, not S3): `PutBucketLifecycleConfiguration` accepts prefix, tag-and-prefix, date and abort-incomplete-multipart rules, and it enforces them. A date rule on `docs/blob/` expired `docs/blob/a` within 60 s and left `docs/log/a`. So the rule works and would remove committed chunks without hesitation. (The multipart abort rule the `s3` crate already assumes is a different, safe case: an incomplete upload is never read.)

## What changed

`<index>/intent` is one object, compare-exchanged with the version the writer last saw, holding up to 16,384 attempts (36 bytes each): slot position, epoch, attempt, op, field, chunk count. Every chunk key is derived from an entry, so nothing is LISTed. A slot with a large string writes it once, before its first chunk. A slot of small fields writes nothing.

- **Open** reads the intent (one HEAD, plus one GET if it exists) before replay. The replay marks the entries a slot names. Entries a slot names are committed and dropped. The rest are pending.
- **Reclaim** removes the chunks of a pending entry only when its position is behind the log head. Slots are create-only, so once the slot at that position exists nothing else can ever be written there: its chunks are certain orphans. An entry at or past the head stays pending, because a late slot write from the crashed writer may still land and name it. Nothing waits on a clock.
- It runs after open and after each group commit, off the acknowledgement path, at most 256 chunks per pass (one HEAD for the version, then the removal). A store that cannot delete turns it off for that writer.
- A definite failure whose delete also fails keeps the attempt pending instead of leaking it.
- The intent compare-exchange also fences a stale writer: after a newer writer wrote an intent, the old one's next large write fails `Fenced` before it writes a chunk.
- Past 16,384 pending attempts the oldest are forgotten. Their chunks leak; nothing a slot might name is ever deleted.

A crash on an idle index leaves its orphans until the log next passes that position (the next write). They cost storage only.

## Cost and latency (RustFS, release build, 30 sequential single-document bulks, provisional)

| Document | PUTs/bulk before | after | p50 before | p50 after |
|---|---|---|---|---|
| 1.2 MiB, 1 chunk | 2 | 3 | 32 ms | 43-47 ms |
| 12 MiB, 2 chunks | 3 | 4 | 286-293 ms | 298-304 ms |

One extra small PUT per slot that has a large string, in series before the chunks (it must be durable first). Group commit shares it across every large string in the slot. Open gains one HEAD. No GET, no LIST. Reclaim happens only after a crash or a failed delete. On real S3 the added latency is one small PUT round trip.

Tests: `a_crash_between_the_chunks_and_the_slot_is_cleaned_up_once_the_log_passes_it` (chunks are kept until the log passes the position, then removed; replay intact; observed failing with the reclaim call disabled), `reopening_never_removes_the_chunks_of_a_committed_slot`, `an_unknown_slot_write_never_removes_the_chunks_it_may_name` (now also checks a reopen removes nothing), `a_slot_of_small_fields_writes_no_intent`, `a_store_that_cannot_delete_keeps_the_crash_orphans_and_stays_writable`, `a_stale_writer_is_fenced_by_the_intent_before_it_writes_a_chunk`, the intent codec round trip and bounds (format), and on RustFS `a_crash_after_the_chunks_is_recorded_and_the_next_writer_removes_them` (child process aborted after its chunks; 4 PUTs, 0 LIST for the next large write).
