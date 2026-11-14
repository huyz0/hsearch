# M03.03 on-disk formats: evidence

Status: **provisional** (4-vCPU Xeon @ 2.1 GHz sandbox VM, 16 GiB, Linux 6.18, rustc 1.97.1, base commit d5cb31f plus this change; single thread, release build; relative numbers only, M02.05 `component` class). Spec: [formats.md](../formats.md). Code: `crates/format`.

## What was run

| claim | command | result |
|---|---|---|
| unit + hardening + golden tests | `cargo test -p hsearch-format` | 2 unit, 26 hardening, 6 golden tests pass |
| lint | `cargo clippy -p hsearch-format --all-targets -- -D warnings`, `cargo fmt --check` | clean; the crate also denies `unwrap`, `expect` and indexing/slicing in non-test code |
| independent encoder agrees | `python3 fixtures/formats/gen.py --check` and `golden.rs` | 17 files (9 good, 8 refusal) byte-identical to the Rust encoder; the generator was written from formats.md, shares no code |
| coverage | `cargo llvm-cov --package hsearch-format` | 95.9% lines, 91.7% regions (floor 90% lines) |

Hardening tests (`tests/formats.rs`): for every kind of object, **every proper prefix is refused, and every byte is flipped three ways (^0x01, ^0x80, ^0xFF), each refused**, plus a trailing byte. Forged lengths and counts (u32::MAX, bound+1, count larger than the bytes present, inline length, external length 200 MiB + 1) are re-sealed with valid checksums so the bounds checks, not the CRC, must refuse them. A 4 GiB body length is refused from the 40 header bytes alone. A 200,000,000-byte external document is admitted, 200 MiB + 1 refused, 9 x 1 MiB inline records refused (slot body bound), 4097 records refused.

## Mutation check (the tests were seen to fail)

Each mutation applied to the source by hand, `cargo test` run, source restored:

| mutation | outcome |
|---|---|
| drop the body-length bound in `Header::parse` | `header_bounds_the_body_before_it_is_read` fails |
| ignore unknown mandatory capabilities | `unknown_mandatory_versions_fail` fails |
| allow a BYOK key on a shared lane | `lane_membership_rules` fails |
| skip the body CRC | the hardening test of every kind fails |
| drop the "count x element size fits in the remaining bytes" check in `Reader::count` | **survives**: the count is still bounded by the per-kind maximum (at most 4096 elements, a few hundred KiB of capacity), and the later reads fail with the same `Truncated`. The check only makes the reserved capacity proportional to the input; no observable behaviour changes, so no test can pin it |

## Benchmark (provisional)

`cargo run --release -p hsearch-format --example bench`, median of 7 runs, min-max range; a range above 25% is flagged unstable (the 64 B, 40 B and 72 B cells are a few to tens of nanoseconds and noisy).

```
hsearch-format bench (provisional; single thread)
crc32c slicing-by-16, 64 B                             18 ns/op   3371.3 MiB/s  range 18..19 (4%)
crc32c bytewise (reference), 64 B                     132 ns/op    460.7 MiB/s  range 118..153 (26%) UNSTABLE
crc32c slicing-by-16, 4096 B                         2156 ns/op   1812.1 MiB/s  range 2151..2171 (1%)
crc32c bytewise (reference), 4096 B                 11655 ns/op    335.2 MiB/s  range 11374..11906 (5%)
crc32c slicing-by-16, 1048576 B                    578377 ns/op   1729.0 MiB/s  range 557939..584600 (5%)
crc32c bytewise (reference), 1048576 B            3010549 ns/op    332.2 MiB/s  range 2930067..3094432 (5%)
crc32c slicing-by-16, 8388608 B                   4490747 ns/op   1781.4 MiB/s  range 4418102..4590297 (4%)
crc32c bytewise (reference), 8388608 B           23765754 ns/op    336.6 MiB/s  range 23313797..24774699 (6%)
slot of 1000 x 512 B records: 542097 bytes
slot encode 1000 records (reused buffer)           315380 ns/op   1639.2 MiB/s  range 309502..322716 (4%)
header parse (40 B)                                    18 ns/op                 range 17..28 (60%) UNSTABLE
slot validate + iterate 1000 records               317716 ns/op   1627.2 MiB/s  range 315476..324069 (3%)
aad (72 B, no allocation)                               3 ns/op                 range 3..5 (49%) UNSTABLE
manifest page of 2048 entries: 139308 bytes
manifest page decode 2048 entries (allocates names)       172898 ns/op    768.4 MiB/s  range 168417..185807 (10%)
mapping decode + SHA-256 verify, 64 KiB             84536 ns/op    739.3 MiB/s  range 82320..114053 (38%) UNSTABLE
```

Reading it:

- **CRC-32C dominates.** Slot encode and slot validation run at CRC speed (about 1.6-1.7 GiB/s): 1000 x 512 B records (542 KB) validate in 0.32 ms, a maximal 8 MiB slot in about 5 ms. Record parsing itself is negligible next to the checksum.
- **Slicing-by-16 beat slicing-by-8**: 1,337 MiB/s to 1,729 MiB/s at 1 MiB (+29%), slot validate 397 us to 320 us, measured in the same session before and after. The cost is a 16 KiB table instead of 8 KiB (both stay in L1), nothing else. Byte-wise is 330 MiB/s, kept only as the test reference.
- **No allocation in the slot path**: `encode_slot` appends to a reused buffer; `SlotView::parse` and `records()` borrow the input and hold no `Vec` (a test asserts the inline payload pointer lies inside the input buffer). This is by construction, not an allocator count: `#[global_allocator]` needs `unsafe`, which the crate forbids.
- Manifest page decode (2048 names) allocates one `String` per entry by design, 0.19 ms per page of 139 KB; mapping decode is SHA-256 bound at about 0.7 GiB/s with the software implementation `sha2` selects on this CPU.
- `MappingObject::decode` reads a mapping up to three times (CRC, SHA-256, then one `to_vec` copy, at most 1 MiB); bounded, and mappings are decoded once per distinct hash per node (PR-43), so it is not on a per-query path.
- Memory is bounded by the per-kind body bound, checked from the 40-byte header before the body is read; no decoder allocates more than that bound plus the element counts, themselves capped.

## Downsides and trade-offs

- **No hardware CRC-32C.** Safe Rust has no intrinsic for it and `unsafe` is forbidden in this crate. A hardware path (SSE4.2 `crc32`, ARMv8 CRC) is typically about an order of magnitude faster and uses the same polynomial, so adopting it later does not change the format. It needs either a vetted dependency (which brings its own `unsafe`) or a scoped crate under a recorded decision; neither is chosen here because the measured cost is small against object-store round trips. NEON/AVX are not used for the same reason. On ARM this build measures nothing; the ARM CI workflow is manual and was not run for this change.
- **A checksum per object plus, later, an AEAD tag** is double integrity on encrypted bodies; M05 decides whether the CRC moves under the ciphertext.
- **Content-hashed mapping names leak equality** of mappings to a bucket lister (PR-43 accepts this).
- **Sorted unique members and file names** mean an encoder must sort; that costs the caller, and buys byte-identical objects for equal sets.
- **Mandatory-only gating**: unknown optional bits and flags are ignored, so an *incompatible* change must use a mandatory bit or a new kind version; a writer that misclassifies a needed feature as optional would be silently misread by old readers.

## Not claimed

No object-store round trip was run (the crate does no I/O); no ARM or AVX number; no memory profile of a 200 MB document (a 200 MB payload is an external object the slot only references, and streaming it is M03.08/M06's); no canonicalisation of mapping JSON; no AEAD.
