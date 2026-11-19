# Early conformance corpus

Machine-readable HTTP cases for the standalone node's selected OpenSearch-shaped surface, in three classes. It makes **no SDK or drop-in promise**: it replays raw HTTP against `hsearch-node` and says nothing about any client or OpenSearch version.

| File | Class | Meaning |
|---|---|---|
| `reference.json` | reference | Behaviour OpenSearch documents and hsearch matches. Expectations come from the documented OpenSearch result on `dataset.json`; `reference_cases` points at a core-v1 case whose reference-run oracle covers the same behaviour. **No case was executed against an OpenSearch server by this corpus.** |
| `difference.json` | difference | An intentional departure recorded in [api-compatibility.md](../../../../docs/plan/api-compatibility.md): the `opensearch` field says what OpenSearch does instead. |
| `unsupported.json` | unsupported | A request outside core-v1. It must be refused by name (typed error, not a 5xx) and change nothing. `temporary_until` names the task that later gives the feature a supported form. |

Each case is `enforced` (must pass today) or `pending` (the profile promises it, the node does not serve it yet; `owner` is the task that delivers it). A pending case must **still fail**: the run that finds one passing fails until the case is promoted to `enforced` and its `owner` dropped, so the corpus cannot drift from what the node does in either direction.

## Case shape

`id` (unique), `area` (a row of `docs/plan/api-profile.json`), `dataset` (load `dataset.json` into `orders` first), `state`, `oracle` (`opensearch-docs` or `hsearch-profile`, plus `ref`), `steps`. A step is `method`, `path`, a `body` (JSON) or `body_text` (NDJSON), and `expect` with exactly one of `status` or `status_in`, and an optional `body` matched as a subset: objects need the named keys, arrays must match in length and position, `"<any>"` accepts any present value, `"<contains:x>"` accepts a string holding `x`, `"<absent>"` demands the key be missing.

## Running

`cargo test -p hsearch-node --test conformance` (add `-- --nocapture` for why each pending case fails). Every case runs on a fresh node over the in-memory store. The same file checks the corpus is well formed: areas exist in the profile, owners exist in `tasks.json`, `reference_cases` exist in core-v1, ids are unique, and every profile area has at least one case.

## Relationship to core-v1

`fixtures/api/core-v1` is the reference-execution corpus and is refreshed separately by M06.05 (D). This corpus is the early, selected set that runs against the node today; it neither edits nor is hashed into the core-v1 manifest. The node serves a v0 subset (no auth, no native `/v1`), so the pending cases are the honest list of what the standalone API still lacks.
