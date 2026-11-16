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
- Chunks go through the ordinary create path, not the multipart `LargeObjectWriter` seam (M03.14, since done), so each chunk is bounded by the 16 MiB payload limit.

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
- **Reclaim** removes the chunks of a pending entry only when its position is behind the log head. Slots are create-only, so once the slot (or the open's fence, M03.08) at that position exists nothing else can ever be written there: its chunks are certain orphans. Since open seals the first absent position, a crashed writer's orphans are removed by the next open. An entry at or past the head (not reachable after a seal, kept as a guard) stays pending, because a late slot write could still land and name it. Nothing waits on a clock.
- It runs after open and after each group commit, off the acknowledgement path, at most 256 chunks per pass (one HEAD for the version, then the removal). A store that cannot delete turns it off for that writer.
- A definite failure whose delete also fails keeps the attempt pending instead of leaking it.
- The intent compare-exchange fences a stale writer in one direction: after a newer writer wrote an intent, the old one's next large write fails `Fenced` before it writes a chunk. If the stale writer writes first, the newer one's next large write fails `Fenced` and must reopen. Safety is unchanged either way (slots are create-only); a stale writer's small slots are not fenced by it.
- A failed removal stops the pass and skips the next 16, so a failing store costs one request a few commits, not 256 on the writer thread per commit. Reclaim runs on the writer thread, so a pass delays the next bulk, not the acknowledgement of the current one.
- Past 16,384 pending attempts the oldest are forgotten. Their chunks leak; nothing a slot might name is ever deleted.

The next open of the index removes a crash's orphans, at most 256 chunks per pass (the rest on later commits).

## Cost and latency (RustFS, release build, 30 sequential single-document bulks, provisional)

| Document | PUTs/bulk before | after | p50 before | p50 after |
|---|---|---|---|---|
| 1.2 MiB, 1 chunk | 2 | 3 | 32 ms | 43-47 ms |
| 12 MiB, 2 chunks | 3 | 4 | 286-293 ms | 298-304 ms |

One extra small PUT per slot that has a large string, in series before the chunks (it must be durable first). Group commit shares it across every large string in the slot. Open gains one HEAD, and one GET when an intent exists. No LIST. Reclaim happens only after a crash or a failed delete. On real S3 the added latency is one small PUT round trip.

Tests: `a_crash_between_the_chunks_and_the_slot_is_cleaned_up_when_the_next_writer_opens` (removed when the next writer opens and seals; replay intact; observed failing with the reclaim call disabled), `reopening_never_removes_the_chunks_of_a_committed_slot`, `an_unknown_slot_write_never_removes_the_chunks_it_may_name` (now also checks a reopen removes nothing), `a_slot_of_small_fields_writes_no_intent`, `a_store_that_cannot_delete_keeps_the_crash_orphans_and_stays_writable`, `a_stale_writer_is_fenced_by_the_intent_before_it_writes_a_chunk`, the intent codec round trip and bounds (format), and on RustFS `a_crash_after_the_chunks_is_recorded_and_the_next_writer_removes_them` (child process aborted after its chunks; 4 PUTs, 0 LIST for the next large write).

# M21.09 evidence (increment 6): where a 200 MiB document's memory goes

Provisional: RustFS 1.0.4 on a shared cloud host, release build, one document of 200 MiB less 1 KiB through `Node::bulk`, resident memory sampled every 100 ms next to the PUT count (the `large` phase of the RustFS profile harness, with a sampler added for this measurement; the harness itself is another change). The 408 MiB at the start is the test's own input (the text and the bulk body).

| stage | resident | what holds it |
|---|---|---|
| input only | 408 MiB | test input |
| source line parsed | 909 MiB | line buffer 200 MiB and the parsed string 200 MiB held together, plus about 100 MiB of parse overhead |
| chunk writes (26 PUTs, 1.5 s) | 825 MiB | the line buffer was still held (`clear()` keeps capacity) until the bulk returned |
| durable ack | 625 MiB | the bulk has returned: its stream (the line buffer) and the parsed copies are released |
| engine `apply` | up to 1,030 MiB | the engine's Document copy (200 MiB) and the indexing chain's per-token occurrence list |
| refresh (flush) | up to 1,489 MiB | postings and stored-field encoding on top of the buffered postings |

The engine alone (a 200 MiB text through `Schema::prepare`, `apply`, `commit`, `refresh`, one process): `prepare` adds 200 MiB (the Document owns a copy of the text), `apply` reaches 1,062 MiB at its high-water mark, 657 MiB above the prepared state. The likely cause, from reading the code and not from a profile of it, is the indexing chain keeping every token as an `Occurrence` of three `i32` (12 bytes) until flush (`indexing_chain.rs`: `per_term`, `PostingEntry.occurrences`). The harness text is `w<n> ` words with n below 5,000, about 6.4 bytes a token, so roughly 33 million tokens, estimated and not counted, or about 400 MiB. That leaves about 250 MiB of the 657 MiB unexplained (the stored-field copy and per-term bookkeeping are candidates). Moving the pin to lucene-rust `f914ae76` (32 commits newer) gave the identical 1,062 MiB high-water mark, so a pin bump alone does not change it. Peak memory for one document grows with its token count whatever the node does around it.

## What changed

The node frees a source line's buffer as soon as it is parsed when it is larger than 1 MiB. The old code held the buffer from the parse until the bulk returned, so the saving is during the chunk writes only: resident memory there went from 825 MiB to 625 MiB, one run each (the 100 ms samples of that window agree to within 15 MiB; no variance across runs was taken) (`a_large_lines_buffer_is_released_once_it_is_parsed_and_a_small_ones_is_kept`, seen failing without the change). The peak is unchanged (1,084 MiB over baseline before, 1,092 after, same harness; noise), because the peak is at flush.

## Not done: the peak (superseded by increment 7: bounded by admission, upstream issue filed)

Bounding the peak needs lucene-rust to stop holding 12 bytes per token. The direction, not yet measured: delta-encoded positions with offsets kept only when the field indexes them, as Java's `TermsHash` byte pools do, or flushing a partial segment's postings while a single document is still being inverted. Either changes `PostingEntry` and its consumers in the codecs and must stay drop-in compatible with Java Lucene's segments (the on-disk format is unaffected; the in-memory type is not). That is upstream work in `huyz0/lucene-rust`, not a change to the writer here; until it lands the node's 32x admission charge (6.25 GiB for 200 MiB) is the bound, measured peak 5.4x.

# M21.09 (increment 7): the 100 MiB validation, end to end on RustFS

Provisional: RustFS 1.0.4 (pinned image), shared 4-CPU cloud container, release build for the numbers (the test also runs in a debug build, 47 s). Not real S3.

## The validation test

`a_hundred_mib_text_field_is_found_by_its_last_term_returned_whole_and_bounded` (`crates/node/tests/rustfs.rs`), over HTTP: one document of exactly 100 MiB (`max_document_bytes`) whose last word is `zebra`. It is indexed, found by `match title:zebra`, and `GET _doc` returns the 104,857,600-byte text whole and byte-equal. Peak resident memory over baseline (high-water mark reset after the input exists) must stay under the node's admission charge for that document, 32 x 100 MiB. A document one byte over the limit is refused 413 and `GET` gives 404 with the index still holding one document. Three release runs: 566.7-566.9 MiB (5.7x) after the write, and the peak is read again after the search and the whole-document GET, so the read path is inside the bound (the figure printed by `--nocapture`). The bound is the node's 32x charge copied into the test as a constant, with about 5.6x slack, so it catches a gross regression and not a small one; the sweep rows are single runs from the profile binary, which no gate runs, and the factor is not monotonic (3.7x to 5.7x). No cgroup cap is enforced: the memory is the in-process high-water mark, which includes the test client's buffers. Seen failing first: with the charge set to 4x the node refused the document with 429 naming the bytes free and needed (so memory is bounded by refusal, not growth); a first draft that put `zebra` against a partial word found 0 hits, which showed the test really searches the last term. Skips without an endpoint unless `HSEARCH_S3_REQUIRED=1`.

## Peak memory against size (`rustfs_profile large <MiB>`, one run each)

| document | durable ack | PUTs | peak over baseline | replay on an empty directory |
|---|---|---|---|---|
| 4 MiB | 0.09 s | 3 | 22.9 MiB (5.7x) | 0.17 s |
| 16 MiB | 0.29 s | 4 | 66.9 MiB (4.2x) | 0.56 s |
| 50 MiB | 0.82 s | 9 | 183.7 MiB (3.7x) | 2.68 s |
| 100 MiB | 1.60 s | 15 | 566.5 MiB (5.7x) | 3.93 s |

No LIST anywhere. The factor stays between 3.7x and 5.7x (5.4x at 200 MiB, increment 6), so the 32x charge is never below the measured factor and is not changed.

## Downside and what stays open

- The 32x charge is about 5.6x the measured peak: admission refuses documents that would fit. A 100 MiB document charges 3.2 GiB, so a node with a smaller budget refuses it with a retryable 429 (a budget that can never fit it says so).
- Engine memory still grows about 5x the text through the indexing chain's 12-byte token occurrences. That is upstream, filed as huyz0/lucene-rust#447 (PR-53); the charge can fall when it lands.
- Replay still has no admission charge (increment 3).

# M21.09 (increment 8): stored text shared and streamed, 200 MiB peak down by 200 MiB

Provisional: RustFS 1.0.4 (pinned image), shared cloud container, release build, `rustfs_profile large 200`, one 200 MiB document through `Node::bulk`. Both sides built and run back to back on the same host; the old pin's two runs and the new pin's two runs are listed, so the spread is visible.

Two lucene-rust changes, both additive and drop-in with Java Lucene's segments: an `Arc<str>` text field the analyzer reads without copying it into its reader (lucene-rust#390), and a stored document of at least twice a chunk written to the `.fdt` slice by slice instead of through a whole-document buffer, byte-identical to the buffered path (lucene-rust#449). `Schema::document` now makes one `Arc<str>` of the text instead of a `String`, which is the one copy the engine's Document holds.

| | pin 17a6254 | pin 52b630e |
|---|---|---|
| peak over baseline | 1,091.8, 1,092.3 MiB (5.5x) | 884.4, 892.2 MiB (4.4x to 4.5x) |
| durable ack | 3.05 s, 4.08 s | 3.17 s, 3.08 s |
| searchable | 8.83 s, 10.11 s | 8.26 s, 8.58 s |
| PUT count | 27 | 27 |

Peak is down about 200 MiB, the stored-field buffer copy; time to searchable is the same within the spread (the old pin's runs differ by 1.3 s between themselves). Request counts are unchanged and there is no LIST. The remaining peak is the engine's single copy of the text, its stored-field value (`Store::Yes` copies once more upstream), and the indexing chain's 12-byte token occurrences (lucene-rust#447, still open), so the 32x admission charge is unchanged.

### What remains to reach the ~1.4x peak target (200 MiB document)

The peak is still about 4.4x the document (884 MiB). Measured on the engine with shared text, `Store::No`, the indexing core adds about 133 MiB over the text, so the engine is not the main remainder. The rest is copies held at the same time:

1. Parse: the request line buffer and the parsed string are alive together (about 200-300 MiB). hsearch work: stream the body into the `Arc<str>` without a second full copy.
2. Stored value: under `Store::Yes` the stored `FieldValue::String` is a second copy (about 200 MiB). Needs more lucene-rust work: an Arc-backed stored value so stored fields are written straight from the shared text (the streamed flush in lucene-rust#449 is ready for it).
3. Buffered compressed `.fdt` on the indexing path (about 100 MiB): the spill and `stream_to` path is not used there yet. lucene-rust work.
4. Occurrence lists (lucene-rust#447, still open).

So yes, reaching ~1.4x needs one more lucene-rust change (item 2, then 3) plus hsearch-side input streaming (item 1).

# M21.09 (increment 9): one copy of the text from the op to the stored value, 200 MiB peak 884 to 488 MiB

Provisional: RustFS 1.0.4 (pinned image), shared cloud container, release build, `rustfs_profile large 200`, one 200 MiB document through `Node::bulk`, two runs each.

Two changes, measured separately:

1. **Pin 9f5aa6d** (lucene-rust#450): a `TextField::new_shared` with `Store::Yes` stores the `Arc<str>` it was given instead of copying it into a `String`, and the stored-fields writer streams it like a `String` of the same text (byte-identical, tested in all three modes).
2. **hsearch holds the text once.** `hsearch_index::Field::Str` is an `Arc<str>` and the engine's `Value` has a `Shared(&Arc<str>)` variant, so the op that carries the text to the writer, the engine's `Document` and the stored value all share one allocation. Before, the engine copied the op's `String` into an `Arc` and the op kept its own copy for the whole bulk.

| | main at pin 52b630e (increment 8) | pin 9f5aa6d | plus one shared copy (this change) |
|---|---|---|---|
| peak over baseline | 884.4, 892.2 MiB (4.4x) | 684.5, 691.9 MiB (3.4x to 3.5x) | 492.1, 484.1 MiB (2.4x to 2.5x) |
| durable ack | 3.17 s, 3.08 s | 2.84 s, 2.78 s | 3.14 s, 3.28 s |
| searchable | 8.26 s, 8.58 s | 7.68 s, 7.89 s | 8.52 s, 9.07 s |
| PUT count | 27 | 27 | 27 |

Time is within the spread this host shows between identical runs (increment 8 listed a 1.3 s gap); the one extra copy made when the parsed `String` becomes the `Arc<str>` is transient and costs about the length of a memcpy. No LIST, request counts unchanged.

## What remains to the ~1.4x target

The peak is 2.4x the document. An allocation trace of the same 200 MiB document in lucene-rust (`shared_text_mem 200 shared`) corrects the earlier estimate that the indexing core adds about 133 MiB over the text: it adds about 270 MiB with `Store::No`, and about 130 MiB more for the compressed `.fdt` that `Store::Yes` holds until commit.

1. **Postings and positions, about 270 MiB (1.35x).** The indexing chain's per-token occurrences and the position encoder's growing buffers (a 64 MiB doubling `Vec` in `for_util::pfor_encode`). That is lucene-rust#447; on its own it keeps the peak above 2x, and it needs an upstream change to the inverter (not something hsearch can shorten).
2. **Compressed `.fdt` held until commit, about 130 MiB (0.65x).** `IndexWriter` compresses stored fields into memory and writes the file at flush; a spill during `add_streamed` needs a destination file chosen at add time, which only the concurrent writer reserves. lucene-rust work, smaller than item 1.
3. **Line buffer plus parsed string at parse time** is already released before indexing (increment 7), so it no longer shows in the peak.

So ~1.4x needs lucene-rust#447 (item 1) and the `.fdt` spill in `IndexWriter` (item 2); no further hsearch change would move the peak.

# M21.09 (increment 10): the stored `.fdt` spills at flush, 200 MiB peak 488 to 400 MiB

Provisional: RustFS 1.0.4 (pinned image), shared cloud container, release build, `rustfs_profile large 200`, two runs.

Pin 1abdba1 (lucene-rust#451): a flush whose documents hold at least 1 MiB of stored values writes the `.fdt` as it compresses instead of building the whole file in memory first. The bytes are the ones the buffered path writes (tested against it, including a `Shared` value).

| | increment 9 (pin 9f5aa6d) | this pin (1abdba1) |
|---|---|---|
| peak over baseline | 492.1, 484.1 MiB (2.4x) | 399.8, 399.7 MiB (2.0x) |
| durable ack | 3.14 s, 3.28 s | 2.63 s, 2.78 s |
| searchable | 8.52 s, 9.07 s | 7.08 s, 7.36 s |
| PUT count | 27 | 27 |

## Correction to increments 8 and 9

An allocation-tracing build was used to attribute the remaining memory, and tracing inflated the figures: the "about 270 MiB of postings and positions plus about 130 MiB of `.fdt`" split in increment 9 is wrong. Uninstrumented, lucene-rust's `shared_text_mem 200 shared` peaks at 337 MiB with `Store::No` and 461 MiB with `Store::Yes` on 9f5aa6d, and 337 and 338 MiB on 1abdba1: the indexing core costs about 133 MiB (0.65x) over the 205 MiB of text, and storing the field now costs nothing extra. The earlier "about 133 MiB" in increment 8 was right.

## What remains to the ~1.4x target

1. **Parse.** The bulk parser makes a `String` of the field and then an `Arc<str>` of it (`Arc::from(String)` copies), so the text is briefly held twice, and the line buffer is alive at the same time. That transient is now the floor at about 2x. Removing it needs a shared text that wraps a `String` without copying (lucene-rust) and parsing the source line straight into that `String` (hsearch).
2. **Token occurrences** (lucene-rust#447): about 0.65x over the text while the field is inverted; delta-coded positions would shrink it.
