# M03.11 fencing on RustFS (first half)

`provisional`: RustFS 1.0.4 (digest pinned in `scripts/rustfs.sh`) in a container in a sandbox. MinIO and real S3 were **not run**; M03.11 stays `in_progress` for them.

## Observed

- **Before the rule.** The shared probe `stale_update_history` (`crates/object/tests/support/mod.rs`) failed (red) against `S3Store` on RustFS: an `If-Match` update from a stale version carrying the bytes the object already held was accepted, because the ETag is a content hash.
- **The rule.** `S3Store::write` refuses a conditional update whose body hashes (MD5, as a single-part ETag does) to the precondition's ETag with `Error::InvalidInput`, before any request is sent. After it, the probe and the whole suite pass on RustFS (15 tests in `tests/rustfs.rs`, 22 in `attribution.rs`).
- **The probe's own checks.** It passes `MemoryStore`, fails a deliberately unfenced control (`Unconditional`), and `the_stale_update_predicate_refuses_a_backend_that_does_not_fence` pins that.
- **ABA is not fenced.** A, B, A then a stale writer holding the first token with distinct bytes is accepted on RustFS 1.0.4 (`aba_is_not_fenced_without_a_token_in_the_payload`): the ETag of the first A is current again. No adapter rule can see that; only a unique per-commit token inside the payload makes every version's hash distinct. The test asserts the observed acceptance, so a provider that starts fencing it fails the test and the commit rule can be revisited.

## Limits

- The rule covers MD5 ETags of single-part objects. A multipart ETag or a provider with another ETag scheme is not refused by it; MinIO and S3 outcomes are unmeasured.
- A caller's legitimate no-op rewrite is now `InvalidInput`; callers that must rewrite put a token in the body.
- The commit rule that carries the token belongs to M06.01 (deliverable added).
