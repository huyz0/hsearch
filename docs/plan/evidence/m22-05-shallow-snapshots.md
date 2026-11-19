# M22.05 shallow snapshots and same-store restore

`provisional`: in-memory store (with a listing wrapper standing in for a provider's LIST), real engine commits on disk, one Linux host, dev builds. No RustFS run, so nothing here is claimed for a real object store. This is a maintenance path, not a query or write hot path, so there is no latency benchmark; the cost that matters is request counts, measured by an operation log on the store.

## What exists

`crates/store/src/snapshot.rs` (`Snapshots`), two key helpers in `Layout` (`snapshot`, `restore_origin`) and a small refactor of `Pins::register` (`register_verified`, so a caller that just read the root does not read it twice, and re-registering an identical pin is idempotent). The records are store-local (magic, version, truncated SHA-256) like the GC boundary; moving them into `hsearch-format` with a `Kind` is a follow-up once M10.01 and M11.06 add their fields.

- **Create.** Per shard: read the head, read the live root, register a pin on it that never expires (`u64::MAX`; released only by delete) through the M06.04 pin API, re-check the boundary. Then write one create-only snapshot record. Pins are registered before the record exists, so a record that exists is always held. A sweep that took the base in between is handled by re-reading the head (three attempts). A failed create releases the pins it registered. Per-shard consistency only: each shard is read at its own moment.
- **Restore (same store).** Registers the new index as a *holder* on the snapshot record (compare-and-swap) and writes a create-only origin record naming source index and snapshot. Nothing is copied. `Snapshots::files` streams a shard's files one manifest page at a time with the key of each object under the **source** index, so the restored index is searchable from the source's objects. Same key only by construction: the restored shards carry the source's key envelope.
- **Delete.** Without holders: mark `Deleting` (compare-and-swap), release pins, remove the record; stopped anywhere, it is refused for restore and the next delete finishes. With holders: `Retired`, pins kept until the last `drop_restore`. A snapshot, a live index and a restored index each hold an object independently, so an object is reclaimed only when none does.
- **Key revocation.** `Keys::usable(envelope)` is checked before any read or write: a revoked key refuses create, restore and file reads with `KeyUnavailable` and zero requests (the key store itself is M05's; tests use a stub).

## Why holders and not a second pin per restore

First version registered a pin per restore. The test that restores after compaction and a sweep failed with `Swept { boundary: 3 }`: the boundary rule refuses a pin on a generation a sweep has already passed, and a snapshot's base is below the boundary by design. Pinning it anew would also have a real race (a sweep that listed pins before the new pin and read the snapshot's pin after its release would delete the base). Holders on the record close both: the snapshot's own pins never move while a holder exists.

## Request counts (per snapshot, one shard; measured by the store's operation log)

| operation | requests | breakdown |
|---|---|---|
| create | 8 (6 reads, 2 writes) | existence check 1, head 2, root 2, boundary 1; pin PUT, record PUT |
| restore | 5 (3 reads, 2 writes) | origin check 1, snapshot read 2; holder CAS, origin PUT |
| delete | 9 (6 reads, 1 write, 2 deletes) | read 2, mark 1, release pin 3, re-read 2, remove record 1 (with holders: 3 and no pin release) |
| drop restore | no LIST | holder CAS plus origin removal, and the delete tail if it was the last holder |

Each further shard adds 6 to create and 3 to delete. Asserted by tests: the 4-document and 128-document indexes (the larger one has more files and pages) make identical request sequences, **no object under the payload or page prefixes is read, written or listed, and there is no LIST or DELETE on create**. Create adds exactly two objects (pin, record); restore adds exactly one (origin). Cost per snapshot at S3 list prices is therefore about 2 PUT and 6 GET/HEAD per shard, with no dependence on documents, segments or bytes. Bound: at most 4,096 shards (a record of about 0.5 MiB) and 64 holders per snapshot; beyond that the call refuses.

## Tests (`crates/store/tests/snapshot.rs`, 15, written first and seen failing against a stub)

Metadata-only fixed cost; record names the root its pin holds and a second create is refused; **compaction twice then a sweep keeps the pinned base and every file restores byte-identical** (bytes and SHA-256 compared with what the engine wrote; control without snapshot loses the base); restore copies nothing and holds what it shares; **a sweep suspended across creation** at the pin write and at the root read (a creator thread paused inside the store while another thread compacts and sweeps): the snapshot ends up on the live base and no pin is left on the swept one; delete with exclusive versus shared objects (two snapshots, then a live base); a delete stopped midway; a revoked key makes snapshot and restores unreadable with no request spent; a failed create leaves no pin and retries; shard-less base; shard and holder bounds; a corrupt record is refused; two restores and a retired snapshot.

## Not done, and downsides

- **Wiring an engine to a restored index** (open reads through `Snapshots::files` keys instead of the index's own payload prefix) needs `publish.rs`/`recover.rs`, which PR 139 owns; a follow-up after it merges. Today a restored index is a verified, byte-identical read path plus its hold, not yet a served index.
- **Detach** (copying to make a restore independent) and the **HTTP endpoints** (M06.05) are not built; the full portable form is M11.06, the quorum-mode tail M10.01.
- **Pins never expire**, so a crashed create leaves them until the same create is retried and the snapshot deleted; nothing lists orphan snapshot pins. Storage retention grows with write churn while a snapshot exists (PR-50 states this).
- One creator per snapshot id (the catalog name claim provides it); two concurrent creators of one id can replace each other's pin.
- A holder registered by a restore that crashed before writing its origin leaks until the restore is retried and dropped.
- Not measured on RustFS; the memory store's compare-and-swap is linearizable, a provider's is the object contract's.
