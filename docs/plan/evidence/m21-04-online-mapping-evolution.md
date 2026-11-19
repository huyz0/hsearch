# M21.04 evidence: online mapping evolution

Provisional: one node over the in-memory store, a 4-CPU cloud container, release builds for the timings. Request counts are exact; timings are relative.

## What ships

- **`PUT|POST /{index}/_mapping`** (`Node::put_mapping`): `{"properties": {...}, "dynamic": ...}`. New scalar fields are appended; a field redeclared exactly as it is is accepted and changes nothing; a field redeclared with another type is refused (400 `illegal_argument_exception`, "cannot be changed from type [x] to [y]; create a new index with the new mapping and use _reindex") and nothing is written. A vector field cannot be added online (refused the same way, pointing to a new index). `dynamic` can be changed online; that adds no version (same fields, another setting), so toggling it names the same two objects (tested).
- **A mapping version is an immutable, content-hashed object.** A mapping carries `version` (one more for each change that adds fields) and the count of fields added since creation, stored as one `#evolution\t<version>\t<added>` line written only for version 2 and later, so every mapping stored before keeps its bytes and hash (tested). Added fields are appended after the declared ones, sorted among themselves; that order is the engine schema order.
- **The engine schema grows.** `Schema::evolve(revision, added)` appends optional fields (scalar kinds only, 14-field native bound, no repeated or existing name, revision forward). `Engine::evolve_schema` swaps it between batches under the engine's own admission lock, so a batch is prepared and applied under one schema. A schema that is not the current one extended is refused. A checkpoint reopens under a schema that extends it, never under one that drops a field or only changes the revision (existing test unchanged).
- **Documents.** A field added after creation is optional: a document may omit it or send null. A slot has a new one-byte `Absent` field tag; a slot shorter than the schema (written before the fields existed) has the missing trailing fields absent. Slots without the tag decode byte for byte as before. Required fields stay required.
- **Old segments.** Segments are never rewritten. They open under the latest mapping; the added field is simply absent from their documents (`get`, search projection, sort and range on the new field all answer; verified).
- **Live swap.** `Index::evolve` runs on the writer thread between slots, then refreshes, so the field is searchable at once. Order in the node: publish the mapping (durable), evolve the index, then swap the mapping that requests build documents from. A document is therefore built under the version before or the one after, never a mix, and never carries a field the writer cannot take.
- **`dynamic: true` now adds fields.** One mapping change per bulk run covers every new field of every document in it. Kinds: string to `text`, boolean to `boolean`, whole number within 32 bits to `integer`; null maps nothing and is not indexed; a float, a larger number, an array, an object, a bad field name or a type another document fixed first refuses that item alone with `mapper_parsing_exception` and adds none of its fields; the field limit (and the engine bound) refuses the item that would pass it. The default stays `strict`.
- **Lost update.** The reference is replaced by compare-exchange against the bytes this node read. A mapping changed by another writer first gives 409 `version_conflict_engine_exception`, and the index is reopened from the store; nothing is overwritten.

## Cost (exact, `put_mapping` tests, counting store)

A change of two fields: **1 create (the new immutable object), 1 HEAD, 1 GET, 1 compare-exchange, no LIST**, identical for an index of 5 documents and one of 1,500. A change that changes nothing, or a refusal, costs 0 requests. A bulk that adds fields for 100 documents makes one compare-exchange. A node that already holds the object (another index with the same new mapping) skips the create.

## Hot path (release, provisional)

- `document()` building a five-field write (`node::tests::document_cost`): 442, 446 ns before; 448 ns after.
- Engine ingest, 2,000 documents of five fields per round, batches of 16 (a throwaway timing, not committed): `prepare` 1,682 to 1,757 ns per document before, 1,852 to 1,962 after (**about +8%**); `apply` 5,962 to 6,176 before, 5,986 to 6,267 after (no change outside noise). Per document the whole path moves about 7.8 us to 7.95 us, **under 2%**. The added cost is the field-position record in each document's digest and the one-pass row check; the schema read in `apply` is an uncontended read lock held only to compare.
- No allocation added per document beyond the larger digest scratch (sized once).

## Trade-offs and downsides

- **Added fields are optional, and a document that omits one cannot be told from one that never had it.** A required field is declared at creation.
- **Dynamic strings are `text`, not OpenSearch's text with a `.keyword` sub-field**: no length cliff, but exact match and sorting on such a field need an explicit `keyword` declaration made before the first document (a field's type cannot be changed afterwards). Chosen over keyword because a keyword refuses a value over 32,766 bytes and the type would then depend on the first value seen.
- **No date detection**, no float or 64-bit integer (the engine has neither kind).
- **The engine's 14-field native bound** still caps an index; the 1,000-field default is not reachable until that is raised.
- **Added fields digest by schema position**, so a digest is not stable across a field being added before it; no digest is persisted.
- **Rollback.** A node from before this change reads an evolved mapping (`#evolution` line) and any slot with the new tag as corrupt. Mappings that never evolved are unaffected.
- A put-mapping forces a refresh (a local flush of buffered documents) so the field is searchable at once.
- Multi-node coordination of a change is the compare-exchange above plus reopen; a quorum or ordering protocol is M07.
- `Resolver` (real-time update) accepts `Absent` for any field and leaves the required check to the engine; M21.06 and M21.05 should read documents that have absent fields.

## Review

Independent review (a separate agent given the task and diff) found no blocking defect and three smaller ones, fixed: the dynamic planner's window now covers the whole batch rather than the first 1,000 items; a document refused for its id or size no longer adds fields; and a dynamic-only change no longer bumps the version, so it cannot mint unbounded mapping objects.
