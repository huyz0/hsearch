# M02 evidence: bulk-path bench, profile and first optimisation

All numbers are `provisional`: one sandbox VM, in-memory store, not a reference machine. They are relative, not absolute.

## What was measured
- `crates/node/examples/bulk_profile.rs`: `Node::bulk` on `MemoryStore`, profiled with `valgrind --tool=callgrind` on a release build with `CARGO_PROFILE_RELEASE_DEBUG=1`.
- `crates/s3/examples/bench.rs`: PUT size sweep, concurrent PUTs and contended CAS against RustFS (run with `scripts/rustfs.sh up`).

## Finding and change
Profile showed `Schema::document` analysing every text field twice for term counting. The change counts analysed terms only when the raw text bytes exceed `MAX_TERMS` (a text of at most `MAX_TERMS` bytes cannot yield more terms than that bound), preserving the limit. Measured effect on the bulk profile: about +13% documents/s (provisional).

Caveat: text longer than 128 bytes is still analysed twice (count, then index); removing that needs the analysed terms passed through, a later task.

Test: `the_term_bound_counts_analyzed_tokens_not_bytes_across_all_text_fields`.

## Next profile targets (not done here)
- serde_json `Value` DOM in the node, about 8-12%.
- per-field `String` copies in `adapter::Operation`, about 3%.
- per-bulk refresh cost, about 14%.
