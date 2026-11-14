# M21.02 evidence: mappings stored once by content hash

Provisional: in-memory object store behind a request-counting wrapper, one node, 4-CPU cloud container. Counts are exact; nothing here is a latency number.

## Design

- **Canonical form.** `Mapping::encode()` is the canonical bytes: a versioned header and one `name<TAB>type` line per field, sorted by name. Parsing drops the order a request spelled its keys in, so two spellings of one mapping are the same bytes. There is no settings body yet (the node refuses `settings` in a create body), so canonical mapping and settings is the mapping alone today; settings join the canonical form when they exist (M21.03 and later).
- **Content hash and shared object.** The hash is SHA-256 of the canonical bytes. The immutable object is `hsearch_format::MappingObject` (envelope, CRC, embedded hash, 1 MiB bound) at `Layout::mapping(hash)`, `v1/local/mappings/<hex>`. The node uses the format crate's decoder, which recomputes the hash and requires the one the referrer named.
- **Per-index reference.** `{index}/mapping` no longer holds the mapping. It holds `hsearch-mapping-ref-v0\n<64 hex>\n`, 87 bytes, naming the hash. That object stays per index (one small reference each); the mapping body, which grows with the field list (up to a 1 MiB object), is stored once.
- **One parsed copy per node.** The node keeps `Arc<Mapping>` by hash (bounded at 4,096 distinct mappings; past that a mapping is parsed per index rather than cached, so the cache cannot grow with the index count). Every index entry holds the `Arc`.
- **Change creates a new object.** A different mapping has a different hash and a different object; nothing is overwritten (create-only writes). The reference of an existing index is not rewritten here: put-mapping and online evolution are M21.04.
- **Existing data.** An index written by an earlier node (the mapping inline at `{index}/mapping`) still opens, parsed per index and not shared. Nothing is migrated.

## Authorization form (the PR-43 isolation requirement)

The form chosen is **reach a shared object only through the index's own reference**, with the object named by its content hash:

1. An index's reference lives under that index's own key prefix, so the authorization that guards the index guards the reference. The shared prefix `v1/<deployment>/mappings/` is read by the node's service identity only and is never granted to index principals (M05 owns that grant; this node reads a shared object only after reading the reference of the index it is serving).
2. The object's name is the SHA-256 of its content, so naming it requires already holding the mapping. A principal of index B who knows only B cannot name A's object, and a guess confirms only that someone holds a mapping the guesser already wrote.
3. Integrity: a reference whose object fails its hash is refused with 500, never served; an existing object under a hash whose bytes differ is refused at create (a collision, or a planted object), never replaced.

What this does not prove: it is not an authorization test of real principals, which cannot exist before M05. It holds the node-level properties: no code path returns a mapping except through the requesting index's reference (`indexes_never_resolve_another_indexes_mapping`), and a swapped, missing or planted object is refused (`a_hash_collision_or_a_tampered_object_is_refused_not_served`). Equality of mappings is visible to anyone who can list the bucket (accepted by PR-43 in `formats.md`).

## Measured (`crates/node/tests/mappings.rs`)

1,000 indexes created from 100 distinct mappings, half spelled with their keys reversed:

| | Count |
|---|---|
| distinct mapping objects stored | 100 (each written exactly once) |
| requests that touched a shared mapping, all 1,000 creates | 100 (one create per distinct mapping; none for the other 900 creates) |
| requests in all, 1,000 creates | 6,100 (about 6 per index: root, reference, engine scratch objects) |
| parsed copies held by the node | 100 (one per distinct mapping), shared by `Arc` across indexes, and again after a restart |

At 1M indexes from 100 mappings the same rule gives about 100 mapping objects and 100 shared-mapping requests, plus 1M references of 87 bytes (87 MB) in place of 1M inline mappings. **That extrapolation is a model, not a run**: the test runs 1,000 indexes and the object count is linear in distinct mappings by construction, because creation is keyed by hash and a node that holds the hash writes nothing.

A cold node (restart, or a second node) pays one extra GET per distinct mapping it meets, the first time: create-only PUT finds the object held, then one HEAD and one GET to compare the bytes (a hash collision check), and on open one GET of the object after the reference. Warm, a create of an already-seen mapping costs no mapping request.

## Trade-offs and downsides

- **One more sequential round trip on a cold open.** Opening an index reads its reference, then, if this node has not seen the hash, the shared object: two dependent GETs instead of one, once per distinct mapping per node. Not on a ranking path (index open, not query), and cached after.
- **The reference is still one object per index.** The saving is the mapping body, not the object count per index. Folding the reference into the index root would remove the object; it changes the root format (M03.03) and is not done here.
- **Orphans.** A create that crashes after writing the shared object and before the reference leaves an immutable object nothing names. It is bounded by distinct mappings, is reused by any later create of the same mapping, and has no collector yet (M03.04 owns authority, pins and GC).
- **Cache is not coherent with out-of-band deletion.** A node that has the object cached will not notice it was deleted from the store. Objects are immutable and referenced, and deletion is a GC concern.
