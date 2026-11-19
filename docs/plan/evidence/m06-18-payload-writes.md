# M06.18 external payload writes

`provisional`: in-memory object stores, local scratch engines, one shared Linux host. No RustFS, no network.

## What exists

- **`hsearch_store::payload`**: `PayloadSink` and `ObjectPayloads<P>`. A batch body over `MAX_INLINE_RECORD_BYTES` (1 MiB) is stored as one object under the index's payload prefix (a fresh opaque name) before the record is committed: one conditional create up to 8 MiB, otherwise a create-only multipart upload in 8 MiB parts (begin, one request per part, complete), sent straight from the caller's bytes. The SHA-256 and length go into the record. A failed upload is aborted and nothing is committed.
- **`Committer::commit_external`** (`crates/commit`): commits a put whose record names an `External { object, length, digest }`. Its receipt digest and `Pending::of_external` are a function of the reference, and `Committer::recover` now resolves an external record by it (it skipped them before, so an unknown outcome on one would have read as not committed).
- **`Serving::with_payloads`** (`crates/store/src/serve.rs`): one hook, `payload::commit`. Without a sink a body over the bound is refused as before (`Rejected(BadBody)`). A payload the store refuses is `Rejected(Store)`: nothing logged, sequence not spent, writer usable. Over the ceiling is `Refused(Limit)`.
- **`engine::batch_len`**: the exact encoded length, so `encode_batch` allocates once.

The batch is the unit that is external, not a field: recovery already reads the whole record body from the object (M06.13), so the format is unchanged.

## Evidence

`crates/store/tests/payload.rs` (11 tests), `crates/commit/tests/commit.rs` (+3), `crates/engine/tests/wal.rs` (+1).

- Documents of 2, 8 MiB minus 100, 8 and 12 MiB written through `Serving` with a real engine recover into a fresh engine to the same stored bytes; requests per write are 2, 2, 4, 4 (create or parts plus completion, plus the slot).
- A history of 2 MiB, 8 MiB, 8 MiB+1, 24 MiB+5, 100 MiB and the 200 MiB ceiling (`CEILING`) is committed and recovered with the recovery buffer only; every body's SHA-256 matches, no request carries more than 8 MiB, and the request count is `parts + 2 per upload`. A removed object, one flipped byte and a one-byte-short object each give `NotReady::Payload(seq)` at that sequence. Not through the engine at 200 MiB: a batch applied by the engine is covered to 12 MiB.
- A batch of exactly 1 MiB is inline (1 write), one byte more is staged (2 writes). Store refusal at the create, a part and the completion leaves the log and sequence alone and a retry recovers. No sink keeps the old refusal. Over-ceiling and empty are refused with no request.

Mutations seen red: external arm of `recover` skipped, inline threshold `>` to `>=`, single-request threshold `<=` to `<`, digest not taken on the small path, digest over the first part only, length check admitting zero in `stage`. Survived: the same length check in `commit_external` (the lane refuses a zero length too, the outcome is the same). Tests were written after the code and then mutated (disclosed departure from test-first).

## Measurements (`cargo run --release -p hsearch-store --example payload_profile`, provisional)

| document | encode | digest alone | stage | requests | largest request |
|---|---|---|---|---|---|
| 12 MiB | 1.6-2.0 ms | 9.5 ms (1.3 GB/s) | 9.4 ms | 4 | 8 MiB |
| 100 MiB | 59-61 ms | 78 ms | 79 ms | 15 | 8 MiB |
| 199 MiB | 120-124 ms | 157-159 ms | 159 ms | 27 | 8 MiB |

Stage time is the digest: framing parts costs nothing measurable. Peak memory beside the document is the document once (the encoded body); a multipart part is not copied. Downsides: the body is hashed in a separate pass (about 0.8 ms per MiB) before the first request, and a payload that uploaded but whose record was refused is an unreferenced object left to garbage collection. `batch_len` made no measurable difference against a buffer that grows (same time and peak resident, Linux `realloc` extends in place); it is kept because it is exact and cheap, not because it won.

`[profile.dev.package.sha2] opt-level = 3` makes the 200 MiB history run in 2.7 s instead of 65 s in a test build.

## Limits

- Provisional: no RustFS, so no real multipart latency. A single 200 MiB document needs a batch of at most `MAX_DOCUMENT_BYTES` including ids and field names, so the node's `MAX_INDEXED_DOCUMENT_BYTES - 512` source bound leaves that headroom.
- A payload whose record was certainly refused (store, stale, not admitted, poisoned) is removed again best effort (one HEAD, one delete); one whose outcome is unknown or whose slot was taken is kept, because the record may be in the log. Test: refused slot leaves no object; a lost create reply is settled by `resolve` (its budget now carries the body size).
- **`sweep` orphan pass hazard (found in review, not changed here):** with `orphan_grace_ms` set (off by default) a payload no manifest names is deleted once old, and a payload only a committed log record names is exactly that until a base covers it. Do not enable it for an index written through this sink until the sweep reads the log's references; wire this before M06.19 turns the sink on.
- Unknown outcome of a multipart completion and the abort of a failed upload are untested (the in-memory store cannot show either).
- The serving host (M06.16) does not install a sink yet; wiring it is M06.19.
