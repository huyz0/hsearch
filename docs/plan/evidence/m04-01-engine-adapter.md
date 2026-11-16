# M04.01 engine adapter and revision isolation

`provisional`: in-memory and local directories on one Linux host; no remote store is involved in this task.

## What exists

- **`EngineFactory`** (`crates/engine/src/factory.rs`): the one place a caller asks for an engine and for what it supports. `open(directory, schema, checkpoint, WriterMode)` returns the native `Engine`; `revision()` reports the pinned upstream revision.
- **Feature inventory**: `Capability` (17 entries) and `EngineFactory::inventory()`. Each capability has exactly one row, supported or unsupported with a stated reason that names its owner (M04.02 range input, A11 scripts, one writer per directory, the selected query IR). `require` and `open` refuse an unsupported capability or mode with `Error::Unsupported` before touching the directory.
- **Exhaustive classification**: `capability_of_query` and `capability_of_sort` match every `Query` and `Sort` variant with no wildcard, so a new variant does not compile until it is classified.
- **`ShardEngine` trait** (`apply`, `refresh`, `commit`, `merge`, `close_boxed`) implemented by the native `Engine`; no Lucene type appears in it. `index` and `node` hold `Engine` values but never open one: a test (`no_crate_outside_the_engine_opens_an_engine_except_through_the_factory`) refuses `Engine::open` and any `lucene_` name in every other crate's Rust source, and `with_local_engine` now opens through the factory. `Snapshot::search` calls `require` for each query and sort capability, so the inventory is consulted at the call site (for `Query::Unsupported` this duplicates the refusal in `query::lower`; it is the hook for a capability the lowering would otherwise accept).
- **Dedicated revision command**: `cargo test -p hsearch-engine` (the whole engine suite is the M02 corpus and old-index gate; `--test bridge_contract` runs the bridge checks alone). It checks that every `lucene-*` dependency in the workspace manifest carries `ENGINE_REVISION`, that no `lucene-rust` directory exists under `crates/`, `xtask/` or `tools/`, that the manifest has no `[patch]` section, and the lifecycle, refusal and corruption cases. Run it after every pin bump.
- Already enforced before this task and relied on: only `hsearch-engine` may depend on a Lucene crate (`tools/workspace.py check`); panic, poison, checkpoint-mismatch and close-failure cases in `adapter_tests.rs` and `adapter_failures.rs`.

## Evidence

- `bridge_contract`: 9 tests green; the full `hsearch-engine` suite green. Mutations seen red: declaring `RemoteRangeInput` supported fails the inventory and refusal tests; a `Engine::open` string added to the node crate fails the bypass gate. The corpus is the existing suite (adapter_public and the other differential tests), not new.
- No hot path changes: the factory is a static table lookup and a delegation, so there is no benchmark to record. Cost: one extra static dispatch per call through the trait object, not measured because no caller uses it yet.

## Why M04.01 no longer waits on all of M03

It consumes the object and conditional-write contracts and their RustFS probe (M03.01, M03.02) and nothing else: the factory opens materialized directories and touches no format, catalog, lane or upload path. `Checkpoint` is the engine's own caller-held retry state, not the M03.03 on-disk checkpoint envelope. M03.14's upload sink is reached by M06.02, not here.

## Limits

- `Error::Unsupported` carries no reason; the reason is in `EngineFactory::support`.
- The inventory is hand-kept: `Capability::index` and the exhaustive query and sort matches make a new capability or shape fail to compile, but a new upstream feature is not detected.
- Corruption coverage is a checkpoint that native state cannot match (typed `Checkpoint` refusal) plus the pre-existing failure suites; no test flips bytes inside a committed segment here.
- The vendoring and patch checks are textual predicates over the tree, not proof about the Cargo graph. A `ShardEngine` trait object has no production caller yet.
