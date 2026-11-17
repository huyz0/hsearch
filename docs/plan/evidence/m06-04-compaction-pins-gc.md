# M06.04 compaction, pins and garbage collection

`provisional`: unit and integration level against the in-memory store (with a listing wrapper that stands in for a provider's LIST and can refuse removals), one Linux host, dev builds. No RustFS here (no Docker daemon), so nothing below is claimed for a real object store. Timings are one run, not a benchmark: this path is maintenance, not the query or write path.

## What exists

- **`Change::Replace { from, root }` (lane) and the head rule (format).** The head could only name a new checkpoint root by raising the cover (`validate_successor`: "a new checkpoint root needs a higher cover"), so a compaction, which rewrites the same history, had no way to be named. A replacement now keeps the cover, must name the current checkpoint as `from` (a replacement of any other root is refused by the head itself), and **raises the publication generation by one**; the old refusal (same cover, different root, same publication generation) is unchanged and still tested. This is a format-level change to the M03.05 head and is the one thing here that is not local to `hsearch-store`.
- **`compact::Compactor`** (`crates/store/src/compact.rs`). `validate_source` requires the head to name exactly the source root, the spec's previous base to be that root, the same mapping and key envelope (`Incompatible`), the same covered prefix (`CoverageChanged`), and a source that still verifies. `compact` then publishes and `finish` names the replacement: it refuses a generation below the collection boundary (`Swept`) and swaps the head by compare-and-swap on the version read (`Publish(Lost)` for a head that moved after the last read). `finish` is public so a stalled compaction resumes at the step that matters.
- **`boundary`** (`crates/store/src/boundary.rs`, `Layout::gc_boundary`). One object per shard incarnation holding a generation that only rises (create, then compare-and-swap on the version read; a lost race to a higher value is success).
- **`pins::Pins`**. `register` creates the durable `PinRecord` (create-only, identity-resolved) and **then re-reads the boundary**; a generation below it is refused and its pin removed (`Swept`). `Pin::usable(now, skew)` is what a reader checks before each use, because collection may reclaim after expiry.
- **`sweep::Sweeper`** (`crates/store/src/sweep.rs`), dry run and sweep over the shards of one index:
  1. read each shard's head and live root (a missing live root stops everything: `LiveRootMissing`);
  2. **publish every boundary (the live generation)**;
  3. only then list and read pins (unexpired ones protect their manifest; expired ones are removed; a pin that vanished since the listing was released and protects nothing; an undecodable one stops the run);
  4. list roots: a root at or above the boundary (the live base and any base still being published) or held by a pin is kept, with its pages and files marked referenced, and any other root is a candidate;
  5. delete what only candidates name, **files, then pages, then the root last**, and the root only when every file and page went; expired pins last.
  A run stopped anywhere (a failed removal, a budget) leaves a root that still names what remains, so the next run finishes. A second run over a clean index deletes nothing.
- **Orphans** (uploads of an attempt whose root was never written) are reclaimed only with `orphan_grace_ms` set, off by default: payloads are per index, so it is only correct when `shards` lists every shard incarnation of the index, and an upload older than the grace that is still in flight loses its objects (its publish then fails `verify_root`, it never names them).
- **Bounds.** `max_tracked` object identities (a sweep needing more refuses before deleting), `max_roots` candidates and `max_deletes` objects a run; `page` keys per LIST. Reads are bounded at 1 MiB each.

## Evidence

`crates/store/tests/gc.rs`, 15 tests (real engine commits on disk), plus the head rule in `crates/format/tests/authority.rs` and `crates/lane/tests/authority.rs`:

- A replaced base is reclaimed (root, page, every file) and the live root stays readable; the dry run changes nothing and reports exactly what the sweep then does; a second sweep deletes nothing.
- A pin holds its base; the instant after expiry it does not (`expires == now` is expired, `now == expires - 1` is not); the expired pin is removed.
- A pin below the boundary is refused and leaves no object; an already-expired pin is refused without a write.
- **A publisher suspended across a sweep** (a compactor publishes, stalls, two other compactions and a sweep run): its root and files are swept, and resuming with the old head read is refused with `Swept { boundary: 3 }`, writing nothing. Through the writer's path (`replace`) it fails on the swept objects.
- A newer base published during a merge (updates and deletes the merge never saw): a compactor with the old head read loses the swap, one with a fresh read gets `SourceMoved`; the newer base stays named.
- Coverage, mapping, key and missing-source mismatches are refused with zero writes.
- **Restart**: for every N from 0 to the object count + 5, the Nth removal fails, then a fresh sweep completes with exactly the live base left, and the live root is readable at every stop.
- A delete budget of 2 splits a sweep into runs that converge; no run deletes a root before its files and page.
- An undecodable pin and a live root that is gone each stop a sweep with nothing deleted; a `max_tracked` too small refuses with nothing deleted.
- Orphans: off by default, kept when younger than the grace, deleted when older, the live base untouched.
- **Race**: three reader threads read the head, pin the generation and verify it, while a compactor makes 14 generations and a sweep runs after each: every accepted pin keeps its base readable for the reads that follow (the assertion), refused pins are retried, and the run accepted pins on every one of 25 repeats. One real defect was found by it: a pin released between the listing and the read made the sweep fail; it is now read as "holds nothing".
- Mutations seen red (each fails at least one test): the pin's boundary re-read removed, pins ignored by the sweep, a root kept only above (not at) the boundary, the boundary check removed from `finish`, the source check removed, the coverage check removed. A mutation that deletes roots even after an incomplete run is equivalent (a spent budget already refuses the root), so no test can see it. **The tests were written after the code and then made to fail by these mutations**, not before.

### Request cost (the sweep of one replaced base, 17 files, dry-run-equivalent counts)

| step | requests |
|---|---|
| reads (HEAD + GET) | 12: boundary, live root, pins, each root (2), kept page, candidate page, the live-root check |
| writes | 1 (the boundary, once; none if it is already high enough) |
| deletes | 19 = 17 files + 1 page + 1 root |
| LIST pages | 4 (pins, roots, payloads, pages), one page per up to 1,000 keys |

Reads do not grow with the number of files (12 for 17); deletes are one per object reclaimed; LIST pages grow with keys under the prefixes, not with elapsed time. With `page = 1` the LIST count is exactly one page per key plus one per prefix (asserted), and a second run over the same state makes the same requests. The 17-file sweep took about 0.3 ms in memory, which says nothing about a provider.

## Independent review (round 1) and what changed

A reviewer who saw only the diff found: `finish` did not re-validate the source, so a stalled compaction resuming on a **fresh** head could replace a newer base (fixed: `finish` requires the head to name the source and generation + 1, and `Change::Replace` carries the expected old root, so the head refuses it too; test `a_stalled_compaction_resuming_on_a_fresh_head...`); a `page` above the provider cap would read a truncated listing as complete, and `page = 0` would loop (fixed: clamped to 1..=1000 and the `Inventory` contract says short pages mean the end); pins trusted the caller's shard and generation (fixed: registration reads the root and compares its own generation, `Mismatch` otherwise); expired pins were collected without a bound (now bounded by `max_tracked`); pins are decoded with `decode_for`. Not changed: every read is a HEAD plus a GET (two requests an object) and kept roots and pages are re-read each sweep, which is the cost shown above; one undecodable or oversized pin, root or page blocks collection of that index by design (an unknown reference is never guessed); and a run stopped by `max_roots`/`max_deletes` can leave a root whose page is gone, whose surviving files only orphan reclaim can then free.

## Limits and downsides

- **Pin and boundary objects are small and untested against a provider's consistency.** Safety rests on the linearizable create and compare-and-swap the object contract already requires, and on `pin_gc`, which is a model, not a proof of this code.
- **A pin that expires under a live reader** is a safety hole the reader closes by checking `Pin::usable` (with a skew margin) before every use; nothing enforces it here (M07).
- **Orphan reclamation needs the complete shard list and a grace** (above); it is off by default for that reason.
- Content equivalence of a merge (the same documents, deletes honoured) is the engine's contract (M04.05); only coordinates are compared here.
- Memory is bounded by `max_tracked` (16 bytes an identity plus the set's overhead) and by one root and one page read at a time; a 2,048 x 4,096-file base needs a larger `max_tracked` to be swept at all, and refuses rather than running out of memory.
- Coverage was not measured locally (no `cargo llvm-cov` here); CI's coverage job decides. `tools/workspace.py check` could not run here (`cargo metadata --offline` needs crates that are not downloaded); CI runs it.

## Follow-up: the S3 inventory and RustFS (same task)

- **`hsearch_object::Inventory`** (moved from the store crate so every provider can implement it without a layer edge): `list(prefix, after, limit)` returns a page (`objects`, `more`) in key order, one version each. The sweep follows `more`, so a provider that returns short pages is read to the end; a page that says `more` and names nothing is an error rather than a loop.
- **`S3Store` implements it** with one `ListObjectVersions` request a page (`key-marker`, `max-keys` up to 1,000, `prefix`), reading each version's id, ETag and `LastModified` (parsed to Unix milliseconds, tested). A page cut by the limit gives up the trailing versions of its last key so a key's versions are never split; the prefixes listed hold create-once objects, one version each. A listed version is for removal, not for a compare-and-exchange precondition.
- **Cost on the wire** (asserted by the in-memory counter test and in `rustfs.rs`): a sweep makes one LIST per page per prefix (pins, roots, payloads, pages: 4 for one replaced base), one DELETE per object reclaimed, and reads that grow with roots and pages, not files. Listing drops from `n + 1` pages to `n` for `n` full pages because `more` replaces the empty trailing page.
- **`Publisher::activate` and `replace` now refuse a base below the boundary** (`Error::Swept`, two reads), so every writer path is guarded and not only `Compactor::finish`. This closes the gap noted in the first version of this document.
- **`crates/store/tests/rustfs.rs`**, 3 tests against a real S3-compatible store: publish, recover, compact, sweep and recover again (the log replays to the same ledger before and after, the replaced root is gone, the live root verifies, LIST and DELETE counts asserted); a pin that holds a base on the store until it expires; and the listing itself (7 objects, 3 a page, exact version ids, `LastModified` within an hour of the clock, removal by listed version). They skip with a message unless `HSEARCH_S3_ENDPOINT` is set and fail when `HSEARCH_S3_REQUIRED=1` and it is not (CI).
- **Not run against RustFS by me.** This sandbox has no Docker daemon and cannot download RustFS, so the three tests printed SKIPPED here; CI's RustFS run is their first execution. The in-memory suites (15 gc, 11 publish, the recover tests) pass locally.

### Review notes on the inventory

- A truncated listing page that holds only delete markers is resumed past (at most 16 times) instead of aborting the sweep.
- `orphan_grace_ms` must exceed the longest upload: on AWS S3 a multipart object's `LastModified` is the time the upload started. The setting is off by default.
- A listing page is bounded by 1,000 entries, not by bytes; a response over the 1 MiB read limit fails the listing, which fails the sweep safely (it deletes nothing from that page).
