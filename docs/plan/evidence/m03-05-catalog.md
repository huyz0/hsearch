# M03.05 evidence (increment 1): structural head, paged catalog, identity resolution

`provisional`: RustFS 1.0.4 (digest pinned in `scripts/rustfs.sh`) in a Docker container on a shared 4-CPU cloud VM, loopback, release build. No MinIO, no real S3, no network. Numbers are relative. Reproduce: `eval "$(scripts/rustfs.sh up)"; cargo run --release -p hsearch-lane --example catalog_profile -- 2000`. Counts are S3 adapter wire attempts (`S3Store::counts`).

## What is built

- **Formats** (`crates/format/src/authority.rs`, [formats.md](../formats.md)): kind 9 `StructuralHead`, kind 10 `CatalogRoot`, kind 11 `CatalogPage`, and the derived keys `lanes/<domain>/<inc>/AUTHORITY`, `catalog/<id>/ROOT`, `catalog/<id>/pages/<bucket>`.
- **Structural head** (`crates/lane/src/head.rs`): `create_head` (create-only, never recreated), `read_head`, `advance_head` with a closed set of transitions (`Establish`, `Checkpoint`, `Schema`, `Publication`, `Membership`). A transition is checked against `StructuralHead::validate_successor` **before any request**: one generation up, new token, same domain, incarnation and mode, activation, writer epoch, membership epoch, schema and publication generations and the cover only rising, the epoch inventory append-only. A lost CAS is `Advance::Lost` and changes nothing; a lost reply is resolved by the write's identity, and if that fails the answer is `Contended` and the caller must re-read. Nothing deletes a head.
- **Catalog** (`crates/lane/src/catalog.rs`): `2^bits` bucket pages (1 to 16 bits) chosen at creation, a name's bucket the leading bits of `SHA-256(catalog || name)`, so a name is found from keys alone. `create`, `delete`, `resolve`, `enumerate` (paged, cursor), `verify`. Each name edit is one conditional write of one page; names in different buckets never contend. A delete leaves a **tombstone** holding the identity it lost, and `create` refuses that identity for that name (`Invalid`); the page successor rule refuses the same thing on the wire format, so a writer holding a stale page cannot do it either. A lost reply is settled by re-reading and deciding again (a repeated create or delete that already happened is a no-op), not by guessing.

## Tests

- Format: 12 tests (every-byte truncation and corruption, forged and over-bound fields, every regression of the head and page successor rules, bucket spread and the page bound against the body limit).
- Lane crate, in-memory store: 17 tests (stale CAS loses and changes nothing, regression refused with zero requests, lost reply resolved by identity, unresolvable reply reported `Contended`, 4 threads x 15 establishes with the property one generation per success and the inventory exactly `1..=60`, 6 threads racing to create one name with exactly one winner, create/delete/recreate cycles from 4 threads, a create whose reply and resolve both fail settled by reading, paged enumeration complete and without tombstones, `verify` finding a misplaced page, request counts).
- RustFS: 2 tests (head CAS is 1 PUT and a stale version loses on the wire; 4 threads racing one name, no resurrection, reopen, zero LIST on the uncontended steps).
- **Seen red by hand** (each mutation made, the named test failed, the mutation reverted): resurrection check removed (`a_deleted_name_never_resurrects_its_old_identity`), `validate_successor` skipped in `advance_head` (`a_change_that_would_regress_is_refused_before_anything_is_written`), inventory append-only rule disabled (`a_successor_never_regresses_anything`), tombstone drop allowed (`successors_of_a_page_follow_the_rule`). They are not kept as a mutation suite.

## Requests and latency (RustFS, two runs, one host)

| step | n | PUT | GET | HEAD | LIST | p50 ms | p99 ms |
|---|---|---|---|---|---|---|---|
| head transition (version held) | 100 | 1.00 | 0.00 | 0.00 | 0 | 6.6-7.5 | 12.3-13.0 |
| head read (100 epochs in the inventory) | 100 | 0.00 | 1.00 | 1.00 | 0 | 2.2 | 3.0-4.0 |
| catalog create, 256 buckets, 2,000 names | 2,000 | 1.00 | 0.87 | 1.00 | 0 | 7.8-8.1 | 12.1-12.6 |
| catalog resolve | 286 | 0.00 | 1.00 | 1.00 | 0 | 2.0 | 2.5-3.3 |
| catalog enumerate, 100 per call | 21 | 0.00 | 13.1 | 13.1 | 0 | 30-32 | 35 |
| catalog create, 4 writers on 2 pages | 400 | 1.47-1.48 | 1.46-1.47 | 2.76-2.80 | **378-382** | 12.8-12.9 | **208-252** |
| `verify`, 256 pages | 1 | | | | 0 | 0.54 s | |

A transition is one PUT and no read because the caller already holds the version; a resolve is one HEAD plus one GET (the adapter's whole-object read). Enumeration visits about 13 buckets a call at this size (2,000 names over 256 buckets is about 8 names a page).

## Downsides, stated

- **A lost CAS race costs a version LIST.** On a 412 the S3 adapter looks for its own earlier accepted write (`find_accepted`, a `ListObjectVersions` plus a HEAD per candidate) before it reports a conflict. Uncontended paths show 0 LIST; four writers on two pages show about **0.95 LIST per create** and a p99 of 208-252 ms against 12 ms. "Hot paths need no LIST" therefore holds for the uncontended path and not under contention on one page. The way out is an adapter option that skips the scan when the caller settles by reading (the catalog does), which is an adapter change with its own downside (an own-write that landed is then settled by a read, not by identity) and is **not made here**; the fan-out (a name's bucket is one of up to 65,536) is what keeps unrelated names from racing.
- **Tombstones are kept for the life of the catalog.** A name used once costs an entry forever, bounded by distinct names ever used, not by churn (a name deleted and recreated 1,000 times is still one entry).
- **The fan-out is fixed at creation.** A bucket holds at most 2,048 entries (the encoder refuses the 2,049th, `TooLarge`, and changes nothing). 65,536 buckets give a ceiling of about 134M names a catalog and a page of about 150 names at 10M. There is no split: a catalog that outgrows its bits needs a new catalog and a copy (not built).
- **Enumeration is by bucket then name**, not alphabetical across buckets.
- **Head read is 2 requests.** A cold open that wants the head pays a HEAD and a GET; the caller that already holds a version pays none.

## Not done (named, not claimed)

- `LaneWriter::open` does not read the head: the `COVER` marker (M03.08) still enforces the cover. The head carries the cover, tail and epoch inventory, and enforcing them from the head instead of the marker needs a checkpoint root, which is M06's. Until then the head's cover is not consulted by the lane.
- Activation routing for a promoted lane (the target's activation head) is not wired; promotion still leaves publication to the catalog's consumer.
- Name to descriptor resolution (the index and shard descriptor bodies) is M05/M06; the catalog resolves a name to an opaque identity only. Alias semantics, case and Unicode normalisation of names belong to the API layer; the catalog accepts 1 to 255 bytes of UTF-8 without a control character.
- `verify` reports a bad page; it does not rewrite one. Repair from the index descriptors needs those descriptors.
- No exhaustive model of the catalog and head transitions (the lane model of M03.04 covers the lane, not these objects).
