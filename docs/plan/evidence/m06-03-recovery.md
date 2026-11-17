# M06.03 bounded recovery and replay

`provisional`: unit and integration level against the in-memory store, one Linux host. No RustFS here (no Docker daemon), so nothing below is claimed for a real object store.

## What exists (`hsearch_store::recover`)

`recover(publisher, target, dependencies, apply)` recovers one index using the object store only. Nothing local is read, so deleting every cache and scratch directory changes nothing (every test builds a fresh `Publisher` over the store and carries nothing over from the writer):

1. Read the lane's structural head. No head is `NoHead`; a head that authorises another activation is `Activation`.
2. If the head names a checkpoint, verify the base with `Publisher::verify_root` (pages, file lengths, mapping and key currency). A base that cannot be verified, or whose object is missing, is `Base`; a transient store error is `Store` (retry). History is never assumed to be absent because an object is unreachable.
3. Stream the committed log from the head's `cover` to the first absent slot with `read_log` (one slot in memory at a time, 2 requests a slot). The index's records go to `apply` in sequence order through the replay barrier (`Replay`), starting at the base's `covered`. Records of other indexes sharing the lane are skipped.
4. A sequence gap is `Gap{expected, found}`; the same (index, sequence) with a different body is `Conflict`; the same body again is applied once; a slot that is not a valid lane object is `Corrupt`; a failing `apply` is `Apply` and is not skipped; a seal record is not a document operation; an external payload is `Unsupported(seq)` (skipping it would lose a document); a malformed operation pairing is `Corrupt`.
5. Bounds: slots, records and body bytes read are limited by the caller; over a bound is `Limit` (take a checkpoint), refused before applying more.
6. **The head is re-read after the scan.** A hole ends the log only if no checkpoint reclaimed it: if the head's cover has moved past where the scan stopped, or its activation changed, the result is `Missing` (the lane's own open does the same).
7. **Readiness barrier.** Recovery returns `Recovered{base, from_seq, applied, replayed, slots, tail, head_generation}` only when the log reached at least the head's tail locator, at least the lane position the caller knows was acknowledged (`Expect::ordinal`) and the applied prefix reached `Expect::seq`; otherwise `Missing`.

## Evidence

`crates/store/tests/recover.rs`, 12 tests, with the log written by the real committer (`hsearch-commit`) over a fenced lane and a base published by `Publisher`:

- Base plus tail: the records handed to `apply` are exactly the receipt ledger's records after the base, in order; replay started at the head's cover (slots read equals tail minus cover), `from_seq` 5, applied 10.
- No base replays from the start; the ledger matches.
- A deleted slot in the middle ends the log early and `Expect` makes that `Missing`; without `Expect` the replay silently stops at the hole, which is why the expectation exists.
- Gap, conflict and duplicate are told apart; corrupt slot (one flipped byte) is `Corrupt`; a failing apply is `Apply`.
- Slot, record and byte limits each give `Limit`.
- Missing head, wrong activation, a base whose file object was deleted.
- A seal record is not a delete.
- A delete reaches `apply` with `delete = true` and an empty body; a put and a delete of one sequence conflict.
- An external payload is `Unsupported`, not skipped.
- Records below the base's position (a cover set lower than the base) are duplicates and are not applied again.
- A checkpoint published and a slot reclaimed ahead of the scan (done inside `apply`) is `Missing`, not a short ready result.

Mutations each seen red: replay from 0 ignoring the cover, the readiness check removed, the conflict check removed, the gap tolerated, the slot bound removed, a seal read as a delete, the head re-read removed, a delete skipped, an external payload skipped. One mutation survives and is equivalent: dropping the delete flag from the duplicate digest changes nothing observable, because a put cannot have an empty body and a delete always does. The tests were written after the code, then made to fail by these mutations (a departure from test-first, disclosed, not repaired).

## Limits

- **In-memory store only**; no RustFS, no real provider, no timing or request-count measurement of a large tail (that is M06.08).
- **The log cannot prove its own completeness.** A slot lost from the middle of the log is invisible to a reader that only follows the log (it reads as the end). Detection needs a position the caller knows was committed (`Expect`), which M07's authority barrier and the client receipts supply; this task takes it as an input and tests that it blocks readiness. Without it, readiness is only as strong as the head's tail locator.
- A pairing the format cannot encode (put with no payload, delete with a payload) maps to `Corrupt` but no test reaches it.
- A repeated sequence is compared only against records seen in this replay; one that repeats a sequence already inside the base is skipped without comparison.
- A sealed index's successor lane is not followed, so a sealed index recovers short of its true prefix unless the caller passes `Expect::seq` (M06.10).
- Single activation: a head that moved to another activation is refused (`Activation`); replay across the activation inventory after a promotion is not done here.
- `apply` receives records; reconstructing engine state (documents, versions, idempotency receipts) from them and opening a reader is M06.10 (split out of this task), and M06.05 wires it into the writer.
- Spill to scratch is not implemented (M06.10): replay streams and is bounded by `ReplayLimits`; a tail over the bounds is refused. The per-record digest map is bounded by `records`, which is the caller's number.
- External (large) payloads are refused with `Unsupported` (M06.10), so a log holding them is not yet recoverable by this module.
- "Delete compute caches/scratch" is shown only in that every test recovers through a fresh `Publisher` over the store; there is no cache or scratch directory yet to delete.
