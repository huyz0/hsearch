# M21.03 evidence: field limits and the dynamic-mapping policy

Provisional: one node over the in-memory store, 4-CPU cloud container. Counts are exact; the one timing is a relative micro-measurement.

## What ships

- **Field limit.** `MappingLimits.max_fields`, default 1,000 (the M02.05 value), configurable up to 10,000 (`--max-fields`). A create body over it is refused whole with 400 `illegal_argument_exception` "Limit of total fields [N] has been exceeded"; nothing is created. The engine schema's own cap (14 fields today) still applies underneath, so the effective limit is the smaller of the two.
- **Depth and nested objects.** `max_depth` 20 and `max_nested_objects` 10,000 (`--max-field-depth`, `--max-nested-objects`), checked by `MappingLimits::check_source` before a document becomes a write: iterative (a 100,000-deep value is refused without recursion), never looks inside a string, so a 100 MiB text field costs nothing, and a document of scalars takes a one-pass fast path.
- **Per-index `dynamic`.** `true`, `false` or `strict` (booleans and strings, as OpenSearch), in `mappings.dynamic`. It is part of the canonical mapping, so it changes the content hash; `strict`, the behaviour before the setting existed, is the default and is **not written** into the stored form, so every mapping stored earlier keeps its hash and bytes (tested).
  - `strict`: the item is refused, `strict_dynamic_mapping_exception`, per bulk item.
  - `false`: unknown fields are dropped before indexing. Unlike OpenSearch they are not retained in `_source`, because this node rebuilds `_source` from indexed fields.
  - `true`: the field limit is checked first (over it: refused whole, naming the limit); within it the item is refused with `mapping_update_unavailable_exception`, because adding a field to a live index is M21.04.
- **No partial mapping change.** Nothing in M21.03 writes a mapping after create, so a refused item, or a bulk with several, leaves the stored mapping untouched; the others in the bulk are indexed (per-item errors).

## Not reachable yet, said plainly

- The bulk reader already refuses any nested object or array before parsing (`shape`), because the mapping holds scalars only. Depth and nested-object limits are therefore unit-tested on `check_source` but cannot be crossed through HTTP until an object or `nested` field type exists (M21.08 and later). The stricter guard stays.
- `dynamic: true` cannot add a field until M21.04; the default is `strict` rather than OpenSearch's `true` for the same reason. M21.04 should decide whether to flip the default (a measured-downside question, not a free one).
- The 1,000-field default cannot be used past the engine's 14-field schema cap; raising that cap is separate engine work.

## Measured (`node::tests::document_cost`, release, ignored test)

Five-field document, 2,000,000 rounds: `document()` 383 ns, of which cloning the source 171 ns (the test clones; the real path moves it); the new checks (shape scan plus unknown-field count) 33 ns, about 9% of the call and none of it proportional to value size. No allocation added on the common path; the unknown-field count replaces the earlier `find`.

## Trade-offs

- A `dynamic: false` index silently drops data a client sent; that is the OpenSearch meaning, but here the dropped field is not in `_source` either.
- The per-item check costs one key lookup per source key, linear in the declared fields (at most 14 today); a map would be needed at 1,000 fields.
