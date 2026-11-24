# Format compatibility matrix

Every durable object starts with the 40-byte envelope (`formats.md`). This matrix states what
each kind promises to a reader of another build. `crates/format/tests/compat.rs` holds every
row to the same checks and fails when this table and the `Kind` enum disagree, so a new kind
cannot ship without a row.

| Code | Kind | Version | Max body (bytes) |
|---|---|---|---|
| 1 | Fleet | 1 | 4096 |
| 2 | LaneDescriptor | 1 | 131072 |
| 3 | LaneSlot | 1 | 8388608 |
| 4 | ManifestRoot | 1 | 524288 |
| 5 | ManifestPage | 1 | 1048576 |
| 6 | Mapping | 1 | 1048608 |
| 7 | Pin | 1 | 1024 |
| 8 | Cursor | 1 | 2048 |
| 9 | StructuralHead | 1 | 65536 |
| 10 | CatalogRoot | 1 | 256 |
| 11 | CatalogPage | 1 | 576512 |
| 12 | IdentityBinding | 1 | 512 |
| 13 | BasePointer | 1 | 512 |

## What every row promises

- A reader of the kind's current version reads that version. A newer version is refused with
  `UnknownVersion`, never read as the current layout, and version 0 is never valid.
- An unknown mandatory capability bit is refused. Flags and optional capabilities are ignored,
  so a newer writer may set them without a fleet gate.
- A body above the bound is refused from the header, before the body is read or allocated.
- An object of one kind is never accepted as another (`Mismatch`), and a kind code beyond the
  table is refused (`UnknownKind`). Codes are stable and never reused.
- Truncation and corruption never decode: the header and body each carry a checksum. The
  every-byte sweeps per kind are in `formats.rs`, not in the matrix test.

## Successor rules are not layouts

M06.04 added a head successor rule, not a layout: the encoding of `StructuralHead` (kind 9, version 1) is
unchanged, so no version rises and the golden fixture stands. A same-cover **replacement** of the checkpoint
root (compaction) now validates when the publication generation rises. Every node that validates head
successors must have this rule before any writer issues `Replace`; an older node refuses such a head
transition as "a new checkpoint root needs a higher cover" and cannot compact or advance past it.

## Changing a layout

Raising a kind's version is a fleet-gated change (`FleetRecord`): the new version is written
only after every node reads it. Update this table and the kind's golden fixture in the same
commit; the test above refuses a stale row.
