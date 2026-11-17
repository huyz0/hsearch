# M06.01 object-WAL commit protocol

`provisional`: unit and integration level against the in-memory store with injected faults, one Linux host. No RustFS here (no Docker daemon), so nothing below is claimed for a real object store.

## What exists (`hsearch-commit`)

A `Committer` over one established `LaneWriter` (the fenced, conditional-create slot log of M03.08). It adds the commit contract on top of the lane and nothing the lane already guarantees is reimplemented:

- **Only the next slot of an established lane commits.** Every acknowledgement is the lane's `append` result: a conditional create of the next ordinal by the writer that holds the fence. The committer never writes an object itself, so an arbitrary uploaded object cannot be acknowledged.
- **Checks before the PUT.** Per request, in order: poisoned lane, body shape (put needs a body, delete none, inline bound), `DependencyView::check` of schema, crypto and resolved fingerprints, duplicate (index, seq) in the call, lane admission (member, not sealed). A refused request writes nothing (`Reject::*`, tested with a store request counter).
- **Dirty-only batches.** Only requests in the call are batched, through the lane `Batcher`, into slots of at most `BatchPolicy` records and bytes. An empty call makes no request of any kind.
- **Receipts.** `Committed(Receipt{index, seq, request_digest, activation, ordinal, batch_position, protocol})`, in request order. The digest covers index, seq, kind and body, so a receipt names what was written.
- **Nothing follows a failed slot.** After the first slot that is not acknowledged the rest of the call is `NotAttempted`; the committer sends one append at a time.
- **Unknown outcomes.** A lost reply the lane can resolve by identity is `Committed`. One it cannot is `Unknown{ordinal}`, the writer is poisoned, and later calls get `Rejected(Poisoned)`. `Committer::recover` reopens from the cover (which fences the log's end, so an in-flight slot can no longer appear) and reports per pending record `Committed(receipt)` (found with the same digest) or `NotCommitted` (absent, or the same identity with different bytes).
- **Old owner after a newer one.** A writer that was fenced by a reopen gets `Rejected(Fenced)` for its next call and acknowledges nothing.

## Evidence

`crates/commit/tests/commit.rs`, 15 tests (the first nine, then mixed rejections across batches with positions resetting, one descriptor read per call for unknown indexes, a fenced writer, a fenced later batch, recover over deletes and repeated pairs, a seal not read as a delete): receipt order and positions; stale schema, crypto and resolved each refused with the store request count unchanged; empty call and one slot per call; bad body, duplicate, non-member and delete; a refused slot leaves later ones `NotAttempted`, the lane usable and no position burned; lost reply resolved by the lane; unresolvable reply then `recover` giving `Committed` for the landed record and `NotCommitted` for the one never sent, then a retry committing at a later ordinal; never-landed slot resolving `NotCommitted` and the old writer then fenced; same identity with a different body not counted as committed. `clippy --all-targets -D warnings` green.

Mutations each seen red: `stopped` never set after a failed slot (2 tests fail), dependency check removed (stale test fails). The tests were written after the code, then made to fail by these mutations.

## Not done here (moved to M06.09)

The per-commit payload is not yet encrypted: the slot body is the request body as given. The deliverable's inline wrapped-DEK envelope needs a fresh DEK per slot (the block sealer uses deterministic nonces, so reusing one is unsafe) and `hsearch-seal` does not cover the metadata store's create/CAS/resolve path. That is its own task, `M06.09`. Also not done, and moved to M06.09: the sparse and hot tenant batching measurements and the payload-plus-head-CAS comparison. Concurrent old and new owners is covered here by a reopen fencing the old writer (two tests), not by a racing multi-writer run; M03.11 holds the fencing evidence; the committer does not yet take a pre-sealed payload. Provider qualification stays RustFS-only per M15.01.

## Limits

- Dependency checking is a trait; the real view (live mapping and key versions) is wired in M06.05.
- One in-flight append per lane is structural (`&mut self` and one `await` at a time), not a runtime lock.
- A request larger than the inline bound is rejected; external payloads are not yet taken by the committer.

- `Reject::Fenced` is not proof that nothing was written: on a provider that cannot resolve a retried create by identity, our own accepted slot reads the same way. Callers settle it with `recover` and `Pending::of`.
- `recover` needs the durable cover as `from` (a record below it reads `NotCommitted`) and callers must keep (index, seq) unique across calls; a repeated pair resolves to the entry whose body digest matches.
- Admission costs at most one descriptor re-read per call, however many unknown indexes it names (tested).
