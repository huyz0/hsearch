# M03.10 adapter multipart upload

`provisional`: RustFS 1.0.4 (digest pinned) in a container in a sandbox, and the scripted in-process provider of M03.07. MinIO and real S3 were **not run**.

## What exists

`S3Store::begin_multipart(ctx, key) -> Multipart` with `put_part`, `complete` and `abort` (`crates/s3/src/multipart.rs`).

- **Create-only.** Completion always sends `If-None-Match: *`; an existing key is `Conflict` and the original object is untouched (RustFS test). Segment files are immutable, so no conditional-update completion exists.
- **Invisible until complete.** HEAD of the key is `NotFound` before completion; an aborted upload leaves nothing.
- **Every request is attributed** through the adapter's `send`: begin and complete as POST, parts as PUT, abort as DELETE, each a logical call and a wire attempt with its bytes. The scripted provider's transport log reconciles with the ledger for a full upload and for an abort.
- **Budgets.** Begin and each part spend the request budget before they are sent (tested for both; complete and abort charge the same way but that is not tested); the default 4 MiB context cannot carry a 5 MiB part, so a caller sizes its budget (the tests do).
- **Part rules before the wire.** An empty part, a part after a short one and completing with no parts are refused with no request. The 10,000-part cap is coded but not tested, and the 5 GiB part maximum is not enforced.
- **No unsafe repeats.** A throttled part is not retried by the retry policy (a status does not prove it did not commit); an unknown completion is `UnknownOutcome` and the caller HEADs the key. A completion answered 200 with an error body is `Unavailable`, not a commit; one without a version id is `UnknownOutcome`.
- **A failed completion leaves the upload open**, so the caller can abort it (`complete` borrows). The upload id is percent-encoded in every query.
- **Abort is idempotent**: an upload already gone (404) is success.

## Tests

`tests/multipart.rs` (7, RustFS) and 7 scripted tests in `tests/attribution.rs`. Mutations each seen red: dropping `If-None-Match`, allowing a part after a short one, not charging the part budget, dropping abort's 404 tolerance, ignoring a 200-with-error body.

## Limits

- Orphans from a process that dies mid-upload are not found or aborted here (no LIST on any path); the bucket lifecycle rule must expire them. Not configured or tested.
- Response budgets for begin, part, complete and abort are fixed reservations (1 KiB, 4 KiB for complete), not the 64 KiB the reader would accept.
- A completion reply lost after commit is not resolved by this module; it surfaces `UnknownOutcome`. Retrying it was not measured.
- Part size is the caller's; no adaptive sizing, no parallel part upload, no resume after a restart.
- The `lucene-store` `BlobStore::writer` that will call this is wired in the engine after PR 47; nothing calls `Multipart` yet.
- MinIO and real S3 behaviour (part minimum, 200-with-error, `If-None-Match` on completion) is unmeasured.
