# M21.07 evidence: nested documents and parent-child joins

Provisional: the numbers are from one shared VM, release build, in-memory directory, one thread, other jobs possibly running. They are relative, not absolute, there is no ARM run, and nothing here ran against RustFS (the RustFS rerun is an open item below).

## What exists

`hsearch-engine` takes two new field kinds, each declared on the schema (`Schema::new` refuses them, so a body is always given):

- `Schema::with_nested(path, fields)` declares an array of objects. Each object is one native document written ahead of its root in the same block (`update_fields_documents(term, block)`), so a root's objects are replaced and deleted with it. Values are `Value::Nested(&[object])`; an object lists its scalar fields in declared order. Limits: 4 paths, one level (`MAX_NESTED_DEPTH`), 64 objects a document, at most `MAX_FIELDS` sub-fields across all paths. Each object counts against `Limits::documents`, so the existing memory and scope bounds still hold.
- `Schema::with_join(name, parent_type, child_relations)` declares one join field: one parent type, at most 4 child relations. Values are `Value::Join { relation, parent }`. Written as Lucene's join does: a relation term plus a `name#parent_type` term and **single-valued sorted doc values** holding the parent's id (a parent holds its own). `Schema::route_key` gives the id a router must hash (the parent's id for a child), and `Schema::check_route` refuses a document placed by anything else.
- Queries: `Query::Nested { path, query, score_mode }` (lowered to `ToParentBlockJoinQuery` over a bit set of roots), `Query::HasChild { relation, query, score_mode, min_children, max_children }` and `Query::HasParent { relation, query, score }` (lowered to `LateJoinQuery`, OpenSearch's own `has_child`/`has_parent` shape, resolved against the snapshot's searcher before the search). `JoinScore` is none, avg, max, sum, min.
- New capabilities `QueryNested`, `QueryHasChild`, `QueryHasParent`, and `CrossShardJoin`, which the inventory carries as unsupported with its reason.
- Named refusals, `Error::Refused(Refusal)`, mapped to a 400 `illegal_argument_exception` by the node: a child without a parent id (its routing key), a parent that names a parent, an unknown relation, a document routed away from its parent (the join across shards), a nested path or join relation the schema does not declare, a nested field outside a nested query or a root field inside one, nested inside nested. Nesting limits are `Error::Limit`.

No lucene-rust change was needed: block joins and `LateJoinQuery` were already at the pinned revision (`df02d694`), and their differential tests against Java Lucene are the upstream ledger's (`docs/parity/joins.md`). The one thing the engine had to learn: the global-ordinals join reads the join field as `SORTED`, so a `KeywordField` (which is `SORTED_SET`) found no join values; the engine writes the join id field as a string term plus a `SortedDocValuesField`.

A schema with no nested path is untouched: no `_hs_kind` field, no root filter, the same bytes and digests as before. A schema with nested paths writes `_hs_kind` on every document and holds any query that could match an object (`All`, an id, a bool with nothing positive) to roots with a filter; a clause on a root field cannot match an object, so those skip the filter.

## Method

`crates/engine/tests/nested_join.rs` runs every query against a plain-Rust oracle over the same deterministic corpus. The oracle applies OpenSearch's documented semantics: a nested query matches inside one object (and the test asserts the corpus has roots where the same two conditions hold only across objects, so the difference is exercised); `has_child` counts matching children between `min` and `max`; `has_parent` takes the parent's match. Corpora: 200 products with 0 to 5 reviews; 60 questions with 240 answers loaded children-first in three segments, then replaced, merged, committed and reopened.

**This is not the pinned OpenSearch run.** The validation line asks for matching the pinned reference results; no OpenSearch is built here. The operators are the upstream-differential-tested ones and the oracle states the OpenSearch rule, but a rerun of this corpus against the M02.01 reference pin is open.

Tests, all passing: 12 in `nested_join.rs` (oracle matches for nested over four author/star bounds, five score modes, nested in a bool, replace and delete take the objects with them, three segments with a replaced block then merge, commit, reopen and read-only open; has_child over five count bounds and has_parent over four topics, with and without score, across segments and on one; a join with a nested object in one schema; every refusal above; the schema declaration bounds). The existing engine and node suites still pass. Seen red: the first run of the join tests found nothing (the `SORTED_SET` finding above) and then failed with a native error across segments until the id field was written `SORTED`.

## Cost (bench, `crates/engine/examples/nested_bench.rs`, 20,000 roots of 5 objects, 5,000 questions and 20,000 answers in 8 segments)

| Measure | Result |
| --- | --- |
| Write, roots only | 0.16 s, 128k roots/s |
| Write, 5 objects each | 0.60 s, 33k roots/s, 200k native docs/s, **3.8x the time for 6x the documents** |
| Root term query, no nested path in the schema | p50 469 us, p99 794 us |
| Same query, schema with a nested path | p50 540 us, p99 995 us (+15%); 1,351 us before the root filter was skipped for clauses that cannot match an object |
| Nested author + stars, score none / avg | p50 587 / 1,026 us, 3,743 hits |
| `has_child`, 8 segments / 1 segment | p50 2,057 / 2,089 us, 2,453 hits |
| `has_parent`, 8 segments / 1 segment | p50 2,984 / 3,037 us, 20,000 hits |
| Plain term on the same join index | p50 619 us |

## Trade-offs and downsides

- Nested objects cost a native document each: writes take about 3.8x as long for 6x the documents, and the index and the `documents` ceiling grow with the object count. 64 objects a document bounds a single write.
- A schema with a nested path pays about 15% on a top-level term query and a filter on `All`, an id and negative-only bools; a schema without one pays nothing. The feature is opt-in by declaration, which is the setting.
- Scoring with `avg` costs about 1.7x `none` here.
- `has_child` and `has_parent` search their from side and build the join field's ordinal map **on every query**, so a join costs 3 to 5x a plain term on this corpus and grows with distinct parent ids. OpenSearch caches global ordinals per reader. Caching the map per snapshot is a follow-up and would need a place to hold it on `Snapshot`.
- Join and nested queries are shard-local. Parent and children share a shard only because the caller routes by `route_key`; the engine refuses a mismatch it is told about (`check_route`) and cannot see a router that never asks. The wiring of that call into the node's bulk path (M07) is not done here.
- Nested and join fields are not projectable, sortable, aggregatable or reachable from the REST mapping: `hsearch-node` renders them in a mapping name but parses neither, and the op log's `Field` carries no nested or join value, so the engine feature is not yet reachable over HTTP (the same state as M23.01).

## Not done here

- The pinned OpenSearch reference run of the shared corpus (above), and the RustFS rerun of the benchmark; ARM.
- Global-ordinals caching per snapshot; `inner_hits`; nested aggregations and sorts; REST mapping and bulk wiring; grandchildren.
