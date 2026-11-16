# M05.03: encrypted block format

Code: `crates/security/src/block.rs`, tests `crates/security/tests/block.rs` (20), independent encoder and vector `crates/security/tests/vectors/block_v1.{py,hex}`, bench `crates/security/examples/block_bench.rs`. Specification: the module documentation and [security.md](../security.md#implemented-contract-the-encrypted-block-format-m0503).

## Red first

The 18 tests of the first run failed against a stub (0 passed, 18 failed) before any codec existed; the rest were added with the mutation round and the review below.

## Negative controls (all run)

- **Independent decrypt**: a test decrypts a sealed object with `aes-gcm` directly, with the layout, nonce and AAD written out by hand from the specification, and checks every header field and the header tag.
- **Known answer**: the sealed object is byte-identical to `block_v1.py`, a separate encoder using Python's `cryptography` package.
- **Every-byte damage**: for every byte of a small (three-block) object, three XOR masks, the whole read path (parse, provider unwrap, header tag, block tags) refuses it. Covers magic, version, suite, flags, ids, sizes, wrapper lengths, wrapper bytes (the provider refuses), header tag and every block.
- **Truncation and extension**: every prefix of the object is refused; one extra byte is refused; dropping the last whole block is refused.
- **Substitution**: a block moved to another position, a block of another object under the same data key, another domain under the same key, a block of an object with a different block count, and one object's header over another's blocks.
- **Object and domain binding**: `Header::open` takes the object and domain the caller's key path named and refuses a whole valid object of another object or domain (removing that check fails the new test).
- **Wrapper**: wrong index at unwrap; a wrapper lifted from another object; a provider-accepted wrapper that unwraps to a different key fails the header tag; a rewrapped key (new provider key version) opens the same unchanged ciphertext; the inline wrapper still reads when no overlay was published; revocation blocks both.
- **Nonce controls**: counters are distinct; counter 0 is never a data nonce (the independent decrypt uses `i + 1`); sealing is deterministic so a retry resends identical bytes.
- **Bounds**: block size 0 and above 1 MiB, more than 2^20 blocks, and each wrapper length one above its bound are refused before allocation; exactly the bound is accepted.

## Mutation round (manual, 18 mutants)

Killed: counter off by one on open and on seal, block-count and block-size bounds, flags, exact object length, ordinal range, prefix and ordinal missing from the block AAD, prefix missing from the header AAD, and the three wrapper-length bounds. Three survived the first run and now have a test: the open-all header comparison, trailing bytes after a header, and the three inclusive wrapper bounds. One survivor is equivalent and stated: the explicit zero-length key reference check, because `KeyRef::new` refuses an empty reference at parse anyway.

## Throughput (provisional: 4-vCPU VM, release, hardware AES, 64 MiB object, `block_bench`)

| Block size | Seal MB/s | Open all MB/s | Open one block (cache-hot) | Overhead |
|---|---|---|---|---|
| 4 KiB | 1,107 | 1,107 | 1.3 us | 0.391% |
| 64 KiB | 1,259 | 1,122 | 29.6 us | 0.025% |
| 1 MiB | 901 | 912 | 385 us | 0.002% |

Small blocks cost tag space (0.39% at 4 KiB) and large blocks cost read amplification for a point read (a 1 MiB block decrypts 1 MiB to return a few bytes), so the block size is a writer choice within the bound, not a constant. The one-block time repeats the same block, so it is cache-hot and optimistic.

## Independent review (round 1) and what changed

No break in the cipher construction. Fixed: object and domain binding was left to the caller (now an argument of `open`); `wrapped()` returned an `Option` that was always `Some`; a silent zero-prefix fallback in `seal_object`; unchecked `ordinal as u32`; claims that said the header tag authenticates the wrapper (it proves the unwrapped key is the sealing key; the provider authenticates the wrapper), and AES-GCM's lack of key commitment is now stated in the module documentation. **Two review points are deliberate deferrals, not fixes:** a move-only fresh-key type that makes key reuse structurally impossible belongs to M05.04, where the writer that owns key creation is wired (a `KeyManager` key is an `Arc` shared with the cache and cannot be consumed here); and the policy that *selects* an overlay wrapper over the inline one, with overlay publication, is M05.05's listed deliverable. This task delivers the property those rely on: any wrapper that unwraps to the sealing key opens the unchanged ciphertext, tested for an inline wrapper, a newer-version wrapper, no overlay published, and revocation.

## Not covered

No ARM run (x86_64 only); no compression; no object-store wiring (M05.04); the rewrap overlay's own format and publication (M05.05); "fresh key per object" is a caller obligation the API cannot check; no constant-time claim beyond the `aes-gcm` crate's.
