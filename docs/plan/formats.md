# On-disk formats (M03.03)

Byte-level specification of every object hsearch writes to the object store. The only encoder and decoder is `crates/format` (`hsearch-format`); the golden corpus in `fixtures/formats/` is produced by `fixtures/formats/gen.py`, an encoder written from this page alone, and `crates/format/tests/golden.rs` requires the Rust encoder to match it byte for byte. Layout decisions this page implements: [M03.09](decisions/M03.09-commit-domain-sharing.md) (lane identity), M03.11 (per-commit token), PR-10/PR-52 (fleet format record, unknown-capability refusal), PR-40 (200 MB documents), PR-43 (content-hashed mappings).

Status: **specified and implemented; formats are not frozen until M03.04's model and M03.08's measurements have run** (see Open items). All integers are little-endian. Nothing here is encrypted yet: the AEAD body wrapper is M05's and is reserved as mandatory capability bit 0, which this build refuses.

## Rules that hold for every object

1. **Lengths are checked before allocation.** Every length and count is compared with its per-kind bound and with the bytes that remain before any slice or `Vec` is made. The header alone bounds the body, so a reader can fetch 40 bytes, learn the exact object size and refuse an oversized object without reading it.
2. **Unknown mandatory things fail.** An unknown envelope version, kind, kind version or mandatory capability bit is refused, never skipped (architecture invariant 10). Unknown **optional** capability bits and flags are ignored by definition (a reader loses nothing it needs), so a newer writer can add an ignorable feature without a fleet gate. v1 writers emit zero for both.
3. **ETags are not hashes.** Integrity is the embedded CRC-32C (corruption) and SHA-256 digests (identity of manifest pages, payloads, mappings). The provider's version token names a version of a key; no API in the crate takes one as a digest, and a test pins that a mapping's hash is neither the checksum nor the hash of the stored object.
4. **Every commit that must fence carries a unique per-commit token** (16 bytes, never all-zero) in the body: fleet record, lane descriptor, lane slot, manifest root, pin. RustFS 1.0.4 accepts a stale `If-Match` write with identical bytes, so two writers must never be able to produce equal bytes. Callers generate the token; a successor that reuses its predecessor's token is refused by `validate_successor`/`with_member`.
5. **Encoders append in one pass** to a caller-owned buffer (header reserved, body written in place, header patched). On refusal the buffer is restored.
6. **Decoders are zero-copy where the result is read once.** `SlotView` borrows the input and validates every record before yielding any; iterating allocates nothing. Small structural objects decode to owned values, bounded by the limits below.

## Envelope (40 bytes, then the body)

| offset | size | field | rule |
|---|---|---|---|
| 0 | 4 | magic | `HSFO` |
| 4 | 2 | envelope version | `1`. Checked **before** the header CRC, because a future layout may not keep the CRC here |
| 6 | 2 | kind | 1 fleet, 2 lane descriptor, 3 lane slot, 4 manifest root, 5 manifest page, 6 mapping, 7 pin, 8 cursor |
| 8 | 2 | kind version | `1..=current`; 0 and newer are refused |
| 10 | 2 | flags | ignored by readers; writers emit 0 |
| 12 | 8 | mandatory capabilities | unknown bits refused (`SUPPORTED_MANDATORY_CAPS` = 0) |
| 20 | 8 | optional capabilities | ignored by readers; writers emit 0 |
| 28 | 4 | body length | at most the kind's bound (table below) |
| 32 | 4 | body CRC-32C | over the body |
| 36 | 4 | header CRC-32C | over bytes 0..36 |

An object is exactly `40 + body length` bytes: shorter is `Truncated`, longer is `Trailing`. CRC-32C detects every single-byte change and every burst up to 32 bits, so a single flipped byte can never decode; the test suite flips every byte of every kind of object three ways.

## Limits

| bound | value | where it comes from |
|---|---|---|
| document, external payload | 200 MiB (`MAX_DOCUMENT_BYTES`) | PR-40 hard ceiling. The per-index default (16 MiB, M02.05) is a policy above this |
| inline record in a slot | 1 MiB | bigger documents are their own object; the batching rule above 1 MiB is M03.08's |
| slot body / records per slot | 8 MiB / 4096 | v0 slot ceiling, below the 16 MiB object contract bound |
| lane group | default 1000, ceiling 4096 | M03.09 decision 4 |
| manifest page / root pages | 2048 files / 4096 pages | a root references up to 8.4M files; worst-case page about 640 KiB under the 1 MiB body bound |
| file name | 255 bytes, no `/`, NUL, `.`/`..`; sorted, unique | |
| canonical mapping | 1 MiB | proposed; PR-42 owns the final number |
| cursor | 32 shard positions (about 1.1 KiB base64) | fits an HTTP header |
| BYOK key id (a KMS resource name) | 512 bytes | |
| residency, engine format | 64 bytes | |
| object key | 1024 bytes | layout test builds the longest of each |
| body bound per kind | fleet 4 KiB, lane descriptor 128 KiB, slot 8 MiB, root 512 KiB, page 1 MiB, mapping 1 MiB+32, pin 1 KiB, cursor 2 KiB | |

## Fleet record (kind 1) — PR-10, PR-52

`generation u64 | write_format u16 | min_read_format u16 | required_caps u64 | token[16]`

Stored at `v1/<deployment>/fleet/FORMAT`, replaced only by conditional write. `generation >= 1`, `1 <= min_read_format <= write_format`. Rules, all in code and tests:

- **Admission** (`admit`): a node refuses to serve when it lacks a required capability, when its newest readable format is below `write_format` (some node may write what it cannot read), or when its **oldest** readable format is above `min_read_format` (the fleet still holds objects it cannot read). Otherwise it may write `min(write_format, its own max_write_format)`. A node therefore never writes a format an older node in the fleet cannot read before the operator raises `write_format`.
- **Successor** (`validate_successor`): generation steps by exactly one, the token is new, `write_format` and `min_read_format` never fall, and `required_caps` may only gain bits. **Raising `write_format` is the point of no return**: once nodes write N+1, a binary that reads only N cannot serve the data, so lowering it would re-admit nodes that cannot read what exists. Rolling binaries back to N is therefore supported exactly while `write_format` is still N, which is when nothing N cannot read has been written; after the raise the way back is forward, to a fixed N+1 binary. (An earlier draft had an operator-set `roll_complete` flag; it was not evidence of what had been written and is gone.)
- **`write_format` and per-kind versions**: every kind is at version 1 and format 1 means "all kinds at version 1". The mapping table (format N to the kind versions it writes) is created with the first kind that goes to version 2; encoders will then take the negotiated format. Today `admit`'s result feeds no encoder because there is only one format. `PolicyTuple.format_version` must be at least 1.

## Lane descriptor (kind 2) — M03.09

`generation u64 | domain[16] | key_kind u8 (1 default, 2 BYOK) | key_id (u16 length, at most 512 bytes) | residency str | durability u8 (1 object, 2 quorum) | format_version u16 | group_id[16] | group_ceiling u32 | member_count u32 | members[16]* | token[16]`  (`str` = u8 length + UTF-8, at most 64 bytes; residency is `[A-Za-z0-9._:-]`, non-empty). `decode_at` requires the domain the key named

Stored at `v1/<d>/lanes/<domain>/<incarnation>/HEAD`. The lane identity is the **policy tuple** (key identity, residency, durability, format version) plus `group_id`. Rules enforced on encode and decode: members strictly ascending (equal sets, equal bytes), at most `group_ceiling <= 4096`, and **a BYOK key never wraps a shared lane** (at most one member), because a revocable key on a shared lane makes lane-mates unreadable after revocation (the M03.09 counterexample). `can_host` admits an index only on an identical tuple and below the ceiling; `with_member` produces the successor (next generation, new token). Descriptor changes only on membership change, never per append.

## Lane slot (kind 3)

Body: `domain[16] | activation u64 | lane u32 | ordinal u64 | token[16] | slot_kind u8 (1 data, 2 fence)` then, for a fence, `successor_epoch u64`; for data, `count u32 (1..=4096)` and records:

`index_id[16] | seq u64 | op u8 (1 put, 2 delete) | payload_kind u8` then nothing (delete, kind 0), `len u32 (1..=1 MiB) | bytes` (put, kind 1) or `object[16] | length u64 (1..=200 MiB) | sha256[32]` (put, kind 2).

Stored at `v1/<d>/lanes/<domain>/<incarnation>/wal/<activation>/<lane>/<ordinal>` with zero-padded decimal fields so lexicographic order is numeric order; the key is derived, never listed. A delete carries no payload, a put always does; an op and payload that disagree are refused. Each record names its `index_id`, which the (future) AEAD binds: **record AAD** = `"hs-lane-record\0\1" | domain | activation | lane u32 | ordinal | record position u32 | index_id` (72 bytes, `aad()`). The AEAD is per record (M05 to confirm), and the AAD binds everything that varies in the key path except the incarnation (which the domain is unique within) and the deployment, so a record cannot be replayed into another slot, lane, position or index. `SlotView::parse_at` requires the position the key named; `ManifestRoot::decode_for` and `PinRecord::decode_for` do the same for their identities. `FleetRecord::admit` also refuses a node whose write format is below `min_read_format`. `FleetRecord` carries no deployment name: copying one deployment's `fleet/FORMAT` over another's is an operator action the key path, not the body, guards. A fence slot carries the same position and token and nothing else; fences are never deleted (architecture, fenced append lane).

## Manifest page (kind 5) and root (kind 4)

Page: `count u32 (<=2048)` then entries `name_len u16 | name | object[16] | length u64 | sha256[32]`.
Root: `generation u64 | index[16] | engine_format str | mapping sha256[32] | covered u64 | crypto_envelope[16] | page_count u32 (<=4096) | pages: page_id[16] | entry_count u32 | sha256[32] each | token[16]`.

`covered` is the checkpoint: slots below it are contained in the files. A page digest covers the page's **whole encoded object**; `verify_page` checks digest, then decodes, then checks the entry count against the root. Key: `.../indexes/<index>/shards/<shard>/<incarnation>/manifests/<id>` and `.../pages/<id>`. Large manifests are therefore a small root plus bounded immutable pages (architecture invariant 5), and a root update is a conditional write with a new token.

## Mapping (kind 6) — PR-43

`sha256[32] | canonical bytes` at `v1/<d>/mappings/<hex sha256>`. The hash is over the canonical bytes only. Decode recomputes and refuses a mismatch; `decode_expecting` also requires the hash a referrer (a root's `mapping`, or the key) named. Identical canonical mappings share one object. **Not delivered here: canonicalisation** (key order, defaults) belongs to the mapping owner; this crate addresses the bytes it is given, so PR-43's "two key orders, one hash" test belongs to that owner. The key name reveals mapping equality to anyone who can list the bucket, which PR-43 accepts; principals reach a mapping only through their own index's descriptor, and the per-principal authorization test PR-43 asks for is M05's.

## Pin (kind 7) and cursor (kind 8)

Pin: `pin[16] | index[16] | manifest[16] | manifest_generation u64 | expires_unix_ms u64 | token[16]`. Cursor: `index[16] | routing_generation u64 | kind u8 (1 vector: count u32 <=32, then shard[16] + ordinal u64 each; 2 pin: pin[16])`. A cursor is a bounded vector or a reference to a durable pin, never an unbounded header (architecture, checkpoints). **A cursor is unauthenticated**: a client holds it and the CRC is forgeable, so a decoded cursor or pin id is never evidence of authorization; the index and pin it names are authorized independently, and the MAC or AEAD that protects it is M05's.

## Object layout

`Layout` derives every key from identities in hand (deployment name `[a-z0-9-]{1,63}`): `fleet/FORMAT`, `lanes/<domain>/<inc>/HEAD`, `lanes/<domain>/<inc>/wal/...`, `indexes/<index>/{descriptor/<gen>, payloads/<object>, pins/<pin>, shards/<shard>/<inc>/{manifests,pages}/<id>}`, `mappings/<hash>`. Under tenant = index, an index's own objects share one prefix; lanes are shared across indexes and live outside it. Nothing is discovered by LIST. This replaces the tenant-level layout in the architecture page, whose `tenants/<tenant_uuid>/` level has no meaning once a tenant is an index; the architecture page is updated to point here.

## Cost of the checks (provisional, measured)

CRC-32C is the dominant cost of encode and of validating a slot, and safe Rust cannot use the hardware instruction. See [evidence/m03-03-formats.md](evidence/m03-03-formats.md) for the table; headline: about 1.7 GiB/s per core, so validating a full 8 MiB slot costs about 5 ms and a typical 0.5 MiB slot about 0.3 ms, against object-store round trips measured in tens of milliseconds. Downside and the way out are recorded there.

## Version identities (requirements, not new types)

- **Opaque version**: the object store's version token (`hsearch-object::VersionToken`) names one version of a key. It is compared for equality by the conditional write that holds it and is never parsed, hashed or used as a digest (rule 3). What a writer must additionally put in the body is the per-commit token (rule 4) and, where ordering matters, a generation.
- **API and profile versions**: a persisted API/profile/schema version belongs in the index and shard descriptor bodies (M05, M06, M08), each of which gets a kind with its own `kind_version` under this envelope; the envelope's mandatory-capability bits are how a profile that changes semantics is made unreadable by nodes that do not implement it. No API-version or profile type is defined here because no such body exists yet.
- **Bounded cursor and pin identity**: delivered (cursor and pin kinds above).

## Open items (not claimed done)

- **Hardware CRC / SIMD.** `unsafe` is forbidden outside `hsearch-module-native`. Hardware CRC-32C (SSE4.2, ARMv8 CRC) would be roughly an order of magnitude faster; the options are a vetted dependency that contains the `unsafe`, or a scoped crate under a recorded decision. Not chosen here: the measured cost is small against I/O and the format does not change either way (same polynomial).
- **AEAD wrapper** (M05) will set mandatory capability bit 0 and move the checksum inside or under the ciphertext; decoders refuse it today.
- **Freeze.** The layouts are ratified for M03.04's model and M03.08's lane implementation to build on; they stay changeable (with a new kind version) until M03.08's measurements say whether slot sizing, the 1 MiB inline bound and the 4096-record cap hold.
- **API/profile/schema version fields** live in descriptor bodies that do not exist yet (see Version identities); not delivered here.
- **Canonical mapping JSON, schema/descriptor bodies (the index and shard descriptors, schema history, catalog pages, job state).** Only their keys are laid out here; their bodies are owned by M05, M06 and M08 and get a kind each when they exist.
