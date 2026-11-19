# M21.05 evidence: real-time get and multi-get from the shard owner

Provisional: one node over the in-memory store, a 4-CPU cloud container, release builds for timings. Request counts are exact; timings are relative and noisy.

## What ships

- **`GET /{index}/_doc/{id}`** returns the latest acknowledged write whether or not a refresh has made it searchable (`realtime=true`, the default). `?realtime=false` reads the searchable snapshot only. `routing` is accepted; an index is one shard on this node, so any routing value reaches it (forwarding is M07.12).
- **`GET|POST /_mget` and `/{index}/_mget`** with `{"ids": [...]}` or `{"docs": [{"_index","_id","_routing"}]}`, at most 1,000 documents, answers in order. A document that cannot be read (no such index, bad id) carries its own error. Documents are read once per index, so cost follows the shards touched.
- **Owner record** (`crates/index/src/live.rs`): the writer records every operation of a durable slot (an `Arc` clone of its fields, no text copy) *before* it acknowledges, and empties the record in the same step as it swaps in the snapshot that covers it (overlay lock then snapshot lock, readers take both in that order). A read takes the record and the snapshot together, so it can neither miss an acknowledged write nor see a half-swapped view. Absent trailing fields (M21.04) are omitted exactly as the snapshot omits them. After an owner restart the replay and its closing refresh cover everything acknowledged.
- **Bounded memory:** at most 16,384 ids and 8 MiB of field data between refreshes; past that the next write group refreshes (emptying the record). If the refresh cannot be taken (all four engine snapshot slots held), payloads are dropped to `Stale` markers and a read of such an id refreshes first; past 65,536 ids the writer refuses new work with a retryable `Busy` rather than grow.

## Cost (exact)

A get or multi-get makes **0 object-store requests**; 10 and 20 documents in one shard cost the same (test `a_multi_get_costs_the_same_for_ten_documents_as_for_twenty`). No LIST.

## Hot path (release, provisional)

- Read, per document: owner record **0.25-0.37 us**; snapshot **about 6 us** (the existing engine id search; `realtime=true` and `false` equal within noise). A multi-get of 100 is 100x one document: no per-call saving beyond the shared lock.
- Durable write, per operation, batches of 256: before **6.2-6.5 us**, after **6.4-7.0 us** at steady state (first rounds 8.0-8.5 us either side; one noisy run reached 8.4-9.7 us). Roughly 0-10%, the record's id insert.

## Trade-offs and downsides

- Up to 8 MiB / 16,384 ids of extra index memory per index between refreshes (fields are shared with the engine's buffer, so the real extra is the map and ids).
- A write group past the bound pays a refresh (a segment flush) on the writer thread, amortized over at least 16,384 ids.
- Each get holds two short locks (shared with the writer's refresh), so a get waits out a refresh in progress, as a search already does.
- A multi-get of snapshot documents is one id search each (about 6 us); a batched engine lookup is a possible later optimisation.
- Concurrent reads hold engine snapshot slots (limit 4) briefly, which can make a refresh `Busy` under heavy get load; existing behaviour for searches.

## Tests

`crates/node/tests/realtime_get.rs` (9) and `crates/index/tests/realtime.rs` (4): write with refresh=false then read; update then get; delete then get; same after restart; absent trailing fields; 10 vs 20 cost; per-item errors; reader threads against a writer never see a version older than the acknowledged floor; bound exceeded by 40,000 unrefreshed writes; shed and refresh-on-read; hard bound refusal. With the record's `remember` call disabled all nine node tests fail (observed).
