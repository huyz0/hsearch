# M20.04 evidence: minimal HTTP node

Provisional: real RustFS 1.0.4 (digest-pinned) on a shared cloud container, debug build, one node.

Crate `hsearch-node` serves a strict subset of the OpenSearch API over HTTP (blocking `tiny_http`, 8 workers): create index with a mapping, `_bulk` (NDJSON, `index` and `delete` items only), get by id, `_search`, `_refresh`. Anything outside the subset is refused by name with a structured error and a 4xx status. The node binds loopback only unless `--insecure-no-auth-listen` is given. An index opens lazily from its stored mapping at `{index}/mapping`, replaying its log on first use after a restart. Per-index limits come from the engine's `Limits::scale()`.

Search lowers an owned JSON query tree to the engine's query type: `match_all`, `term`, `terms`, `range`, `match`, `match_phrase`, `bool`, plus `sort`, `from`/`size` (window 1000) and `_source`. Totals are exact (the engine's collector is inexact past 1,000, so an exact count runs). `_score` is `null`: scoring is not part of this subset.

Checks (`crates/node/tests/http.rs` on the in-memory store, `crates/node/tests/rustfs.rs` on RustFS, raw TCP client, no SDK):

- 100,000 documents bulk-indexed over HTTP, searched, the node stopped, a new node started on the same bucket with an empty local directory, and the same searches answered identically.
- One over the bulk item limit and one over the byte limit refused whole (413); at the limit accepted. Items fail alone with their own status.
- Routes, parameters and bodies outside the subset refused by name.
- LIST is zero on every path, counted from `S3Store::counts`.

Measured, provisional: 100k documents over HTTP about 18.5 s; restart with full log replay about 11.5 s.

Review fixes: ids are checked per item (1-256 bytes) and a chunk the write path refuses is bisected, so a bad item fails alone; each index opens under its own lock, so a cold index does not block others; a writer that stopped (fenced, poisoned, closed, apply failed) is evicted and reopened from the store on the next request; a create retry that finds a root without a mapping completes it.

Not in this slice, and nothing in a response claims it: authentication or TLS, dynamic mapping, generated ids, `create` and `update` bulk actions, scoring, aggregations, scroll, real-time get (get reads the latest searchable view), segment-based writer recovery (restart replays the whole log), atomic index creation (root then mapping), publish over HTTP, socket read timeouts (eight stalled connections can occupy every worker, so the node belongs on loopback or behind a proxy), and hit order without a `sort` (engine-defined, not relevance, since `_score` is null). Comparison against the engine differential documents under `fixtures/engine-research` is not done: no result documents exist for this query subset yet.
