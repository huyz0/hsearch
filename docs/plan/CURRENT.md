# Current state

One page. Live task status is generated from `tasks.json` (`python3 docs/plan/tools/plan.py next` and `summary`; CI writes the summary on every run). This file holds only what changes rarely.

## Requirements
[product-requirements.md](product-requirements.md) is the authoritative product requirement list (confirmed 13 Nov 2026). It wins over older plan text where they conflict.

## What exists
- Workspace crates: `hsearch-types`, `hsearch-observe`, `hsearch-engine` (bounded Lucene adapter over `lucene-rust`), `hsearch-object` (provider-neutral object/CAS traits plus in-memory fault double), `hsearch-testkit`.
- Differential tests against Java Lucene fixtures under `fixtures/`, rendered by `crates/engine/tests/render_adapter_fixtures.py`.
- API contract: `docs/plan/api-compatibility.md`, `api-profile.json`, `python3 tools/api_contract.py check`.
- No search service or HTTP API yet.

## Harness: frozen
The M01 harness (sandbox, task runner, review gate, navigation) is accepted as built. Its follow-on tasks (M01.08, M01.17, M01.20, M01.21) are cancelled. Do not add harness tasks; harness work (tools, CI, gate code) above ~20% of recent commits holds harness tasks in `plan.py next`; CI `harness-budget` only warns. Product docs do not count as harness work. Old coverage, source-proof, forensic-retention and ledger history is in `docs/plan/archive/`; it is not required reading.

## Next product goal
A first runnable slice, kept small: **M03.02** (probe real RustFS) → **M20** (S3-compatible adapter, engine over object-stored segments, write path v0, minimal HTTP endpoint, end-to-end qualification). M20 writes v0 formats that are disposable until M03.03 freezes them, and has no auth, BYOK or multi-node; it lists those gaps itself. M02 and M03 keep going beside it.

After the slice: M04 to M06 harden it into the durable store (M06.07 adds the refresh modes), then M21 (documents and mappings), M22 (PIT/scroll, aliases, reindex, by-query jobs, shallow snapshots), M23 (vectors) and M07 onward (nodes, roles, failover, drain) with M24 (extension modules) after the roles exist. Pick work with `python3 docs/plan/tools/plan.py next`; product tasks are listed first.

## Requirements to tasks
[product-coverage.md](product-coverage.md) maps every PR requirement to the tasks that deliver and prove it, and lists the decisions still open (commit-domain sharing under tenant = index in M03.09, the recovery and drain targets in M02.05, vector gaps in M02.12, the WASM runtime in M24.03).
