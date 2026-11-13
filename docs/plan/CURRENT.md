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
The M01 harness (sandbox, task runner, review gate, navigation) is accepted as built. Its follow-on tasks (M01.08, M01.17, M01.20, M01.21) are cancelled. Do not add harness tasks; harness work above ~20% of recent commits is held (`plan.py next`, `plan.py harness-share`, CI `harness-budget`). Old coverage, source-proof, forensic-retention and ledger history is in `docs/plan/archive/`; it is not required reading.

## Next product goal
RustFS-backed object store → engine over it → a minimal index/search endpoint (M03.02 onward, then M06). Pick work with `python3 docs/plan/tools/plan.py next`; product tasks are listed first.
