# M03.14 large-object upload seam

`provisional`: memory double, the scripted provider and RustFS 1.0.4 (digest pinned) in a container in a sandbox. MinIO and real S3 were **not run**.

## What exists

- **`hsearch_object::LargeObjectWriter`** (additive, `crates/object/src/upload.rs`): `begin_upload`, `put_part`, `complete_upload`, `abort_upload` in the contract's `Operation` style. The upload handle is an associated type that borrows nothing, so a writer that outlives a call stack holds it beside an `Arc` of the store. Implemented by `MemoryStore` (reference, bounded by its 16 MiB payload limit) and `S3Store` (over the M03.10 `Multipart`, now a store-free handle).
- **Shared probe** `upload_history` (`crates/object/tests/support/mod.rs`): invisible before completion; exact concatenated bytes and length; a part after a short part and an empty part refused; an existing key is `Conflict`, the object untouched and the upload still open; abort idempotent and leaves nothing; an aborted upload cannot complete; an empty upload is not an empty object. Green on `MemoryStore` and on real RustFS; a deliberately broken writer whose completion creates nothing is rejected by the same predicate.
- **`hsearch_engine::ObjectUploadSink`** (`crates/engine/src/upload.rs`): an `OutputSink` for a write-capable `BlobStore::writer`. It holds at most one part (`part_size`, at least 5 MiB). A file of one part or less is one conditional `create`; a larger one is a create-only multipart upload. A part that fails aborts the upload and latches the error; a sink dropped or whose completion fails aborts what is open.

## Measured

- Memory: peak buffer *capacity* (not just length) equals `part_size` for 64 KiB writes and for one enormous write (memory double, 10.5 MiB file); growth doubles but is capped at `part_size`. A file of one part or less is copied once more into its single request (`Payload::new`), so that path holds up to two copies of a file of at most one part.
- Real RustFS: a 40 MiB + 1000 byte file written in 256 KiB chunks through the sink: peak buffer 5 MiB, 9 part PUTs and 2 POSTs on the store's own ledger, zero retries, the length correct, and bytes either side of every part boundary and at the end read back equal.
- Mutations each seen red: aborting nothing on drop, nothing after a failed completion, flushing at a full buffer with no more bytes, not latching a failed part's error, `finish` after a failed part publishing the leftover buffer (found in review: it committed a truncated object before the fix), aborting after an unknown completion, letting the buffer grow past `part_size`.

## Limits

- The sink is not yet reached: the publish path (M04, M06.02) owns which `BlobStore` and which manifest a segment goes to, and its `writer()` must return this sink. `ObjectBlobStore` stays read-only by design.
- A small file's `create` uses a request id derived from the key alone by FNV-1a (stable across builds, pinned by a test and cross-checked in Python), so the same bytes written twice to one key are a replay and succeed; different bytes under a taken key are refused (`AlreadyExists`), small or large, on real RustFS. **A multipart file has no replay**: the same bytes rewritten to an existing key are `AlreadyExists`, so the two size classes differ on an exact rewrite.
- After an unknown completion the sink does not abort (the object may exist and an abort could race it); the caller reads the key, and the orphan is left to the lifecycle rule.
- `MIN_PART` and `MAX_PARTS` are S3's limits applied to every provider; another provider's would need them declared on the trait. `MemoryStore` models no part-count limit beyond its 16 MiB payload bound.
- Orphan uploads from a process that dies mid-file are not found or aborted (no LIST on any path); the bucket lifecycle rule must expire them, not configured or tested.
- Parts go one at a time, with no parallel upload, resume or adaptive part size.
- MinIO and real S3 behaviour of multipart is unmeasured.

Status: done on 16 Nov 2026. M03.10 is settled on RustFS (the MinIO leg moved to M15.01), so this task's dependency is met; the multipart behaviour on MinIO and real S3 is measured in M15.01.
