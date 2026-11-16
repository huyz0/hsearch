# M05.04: encrypt every persistent data path

Code: `crates/security/src/readiness.rs` (tests `crates/security/tests/readiness.rs`, 10) and the new crate `crates/seal` (`SealedBackend`, tests `crates/seal/tests/seal.rs`, 21, bench `crates/seal/examples/seal_bench.rs`); `block::object_id` in `crates/security`. Contract summary and spill inventory: [security.md](../security.md#implemented-persistent-path-readiness-and-the-sealed-object-backend-m0504).

## Red first

Readiness: 7 of 9 tests failed against a stub that reported no findings and a full claim (the 2 that passed assert only a stub-compatible outcome). Sealed backend: 14 of 14 failed against a stub returning `Unsupported`. `object_id` was written with its test; the test was then seen red by mutating the hash label.

## What was inspected on disk

The sealed-backend test reads the bytes the store holds, not what the decorator returns: a 820-byte plaintext of a repeated marker never appears in them, the object starts with `HSEB`, and two objects with identical plaintext under different keys share almost no ciphertext after the header. Every one of the stored object's bytes, flipped one at a time on the read path, makes `head` or the read fail (no byte is ever returned unauthenticated).

## Negative controls (all run)

- Readiness: plaintext scratch refused in production (and only reported in development); a declared encrypted volume the probe cannot confirm is refused; scratch declared sealed is refused for both scratch classes; each of the six paths removed in turn is a finding naming it; two locations for one path are each checked; plaintext scratch or journal caps the claim at objects-only and plaintext objects at none.
- Sealed backend: a sealed object copied to another key of the same index and to another index is refused; a length the header disagrees with is corruption, on first open and from the cache; a read at a version that is not the stored one never reads it; objects cut at every header offset are corruption, not a panic; revoking one index's key blocks that index and leaves another readable; unknown scope, a non-`Absent` precondition and a bad configuration are refused before any provider call.
- Retries: an identical retry resends identical bytes and wraps once; a retried identity with changed plaintext is an identity conflict and wraps nothing; with the retry memory limited to one entry an evicted retry fails closed at the store and never produces a second ciphertext of that identity; a sealed request over the byte bound is not remembered.
- Key-call budget: one wrap per object; 200 range reads make no provider call; a fresh decorator over a warm key cache makes at most one unwrap.

## Mutation rounds (manual)

Readiness, 12 mutants: all killed. Sealed backend, about 25 mutants: all killed but two equivalent ones, stated: the explicit short-length check (`stored_len < HEAD_LEN`; the header parser refuses the same input) and the version comparison before the open (the in-memory store already refuses a version it does not hold, so the check is defence in depth against a backend that does not).

## Cost (provisional: 4-vCPU VM, release, in-memory store and in-memory KMS, so this is CPU only and the plain baseline is a memcpy)

| Block size | Read | Plain (us) | Sealed (us) |
|---|---|---|---|
| 4 KiB | 4 KiB | 1.1 | 3.8 |
| 4 KiB | 256 KiB | 10.2 | 107 |
| 64 KiB | 4 KiB | 0.8 | 22.8 |
| 64 KiB | 256 KiB | 10.0 | 122 |

A point read decrypts a whole block, so a large block makes a small read slower (64 KiB blocks: 4 KiB read costs 29x a plain memcpy); a small block costs tag bytes and more blocks per large read. The block size is a writer's choice. Against a real object store a first-byte latency of tens of milliseconds dwarfs these microseconds; the request count is what matters, and it is unchanged after a `head` (one backend read per range), plus one header read per uncached object.

## What this does not claim

- `SealedBackend` is **not wired into the node's write path**; no caller uses it yet. Decorating `MetadataStore` mutations (lane slots, heads, manifests) is not done and is where the M03 envelope's AEAD capability bit still applies.
- There is **no real `VolumeProbe`** (dm-crypt, LUKS, cloud-volume flags); the trait and the policy are tested with a fake. M15 qualifies a probe per platform. No node startup calls `enforce` yet.
- The journal, persistent cache and replay spool do not exist yet, so only their declaration slots are checked.
- Engine scratch is plaintext today unless the operator puts it on an encrypted volume; production refuses to start without a confirmed one, and that refusal is the whole protection.
- A fresh-key type that makes key reuse structurally impossible is still not delivered; `create_object_key` hands the decorator a new key per call and the decorator seals once per identity.
- x86_64 only; no ARM run.
