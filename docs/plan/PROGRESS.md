# Progress and handoff

Last updated: 1 November 2026 (Australia/Sydney).

## Current state

The repository began as an empty directory without `.git`, source code, tests, or workflows. This work delivers the research and planning package. The active ledger contains **18 milestones (M00-M17) and 118 tasks**. M00 is complete and M01.01 workspace bootstrap is under review; the other 110 product tasks remain planned. The operational AI harness, storage/search runtime, cloud integrations and benchmark targets are not delivered. No inherited-project test result is claimed.

The user paused the initial research turn and then resumed it with the added requirements for module extension points, cross-cluster replication/copy and portable BYOK. The paused turn made no filesystem progress. Resumed work inspected the authoritative empty directory, retrieved source evidence and then created these planning artifacts. Source inspection changed the critical path by exposing the whole-file engine input limitation and the fork's incomplete quorum activation.

## Public hosting and workspace bootstrap

The original planning package is published at [huyz0/hsearch](https://github.com/huyz0/hsearch), with baseline commit `1ac6cbb449d7193f019c34b13ad42d44c05a36f9`. The authenticated owner and public visibility were verified before publishing. An independent review found no blocking secrets/private-data/publication issues and reproduced all existing Python checks and both cost-research artifacts. No project redistribution license has been selected; public visibility does not grant one.

M01.01 is under review on `codex/m01-workspace`, using an isolated worktree based on that commit. It owns the Cargo/toolchain/configuration, initial types/observe/engine/testkit/xtask crates, dependency policy, bootstrap CI and development evidence. Shared ledger edits are serialized by the integrator. [Current executable commands and limits](development.md) distinguish this bootstrap from an operational search store or completed AI harness. The initial Linux check rejected missing documentation in an integration-test crate; documentation was added before rerunning. No tests or warning gates were weakened.

[Local Linux evidence](evidence/m01-01-bootstrap.md) records the complete passing bootstrap suite and 43/43 production lines covered. Independent final-tree review and native hosted CI are pending.

All broader M01.02-M17 obligations remain open. Full product delivery still requires actual storage/security/query/replication implementation, independently reviewed correctness evidence, provider credentials/billing, native hardware and release-scale/soak qualification. No release or deployment has occurred.

## Immediate cost research checkpoint (planning history)

The user requested lower S3/API and cross-AZ costs, then corrected the first extension: research belongs now, not in a late M18. M00.07 delivers [source-backed decisions](research/cost-decisions.md), [72 seeded batching experiments and a bounded fence model](evidence/cost-research-results.md). The [cost economics](research/cost-economics.md), [operating profiles](operating-profiles.md) and executable count model remain supporting material for R23.

The selected object WAL uses ordered conditional-create slots in established fenced lanes. It removes a mandatory second head CAS and initial key-sidecar PUT per batch. Wrapped DEKs are inline; KMS wrapping is still charged. Strict single-in-flight append, failure poisoning, activation fencing, retained fence/tail keys and authoritative latest-read barriers are required. Structural checkpoints and segment publication remain separate, amortized operations.

At a 250 ms wait, compatible tenant/key batching produced about 14.3 times fewer batches for the busy synthetic workload, but only about 6% fewer for the sparse workload. These are batch counts and waiting times, not service latency or cloud TCO. The correct bounded fence model explored 23 states; three unsafe variants each produced a counterexample. Its limits include full activation/reopen, multiple epochs, real provider semantics, GC and crypto, which have explicit early implementation acceptance tests.

M18 is retired. Its eight tasks moved to M02.07, M03.07/M03.08, M07.07, M09.08, M15.06/M15.07 and M16.07. [Old links and IDs](milestones/M18.md) remain a mapping only. M17 release depends on the migrated obligations. M02 calibrates the researched choices; M03/M06 implement and prove the protocol; M07/M09 deliver locality and budget controls; M15/M16 qualify actual cloud bills and operations. Later milestones verify the implemented design rather than postpone the initial research.

`object-resilient` remains the baseline, with 250 ms as the initial benchmark batching cap; economy starts at 1 s. These are tuning seeds, not end-to-end latency promises. Regional object durability and zone-local serving avoid routine cross-zone full-payload replication. The low-latency HA/zonal quorum alternatives declare their own failure/latency/capacity contracts. Budget pressure cannot weaken an accepted contract.

Current phase: M00.07 research and final evidence/ledger audit completed. M00 is done; the other 17 milestones are planned. At that planning checkpoint, seven planning tasks were done and all 111 product tasks remained planned; the newer bootstrap checkpoint above supersedes that status. The [immediate research audit](evidence/cost-research-now-audit.md) and [observed validation](evidence/cost-research-now-validation.txt) own this revision. The first cost-extension evidence is retained as a historical snapshot.

## Decisions and findings

- Native Lucene Rust remains the core, pinned initially to `46e280bc2a208544fb36ecd95932e89cd8bc5618`. Remote input/codec integration is a required spike and implementation gate, not an assumed adapter feature.
- Object-durable commits establish the first RustFS end-to-end baseline. A separate quorum journal module delivers low-latency majority-fsync writes with explicit archive-lag exposure and authority migration.
- Immutable manifests/segments, ordered fenced WAL slots, resolved operations, separate reader checkpoints and coordinated pin/GC boundaries form the correctness core. Latest reads pay for an authoritative committed barrier/tail, rather than trusting a stale structural manifest.
- Copy and CCR are independent optional modules over common snapshot/change-stream/import contracts. Independent destination storage/keys and retention-expiry/promotion semantics are required.
- BYOK uses application envelope encryption with pluggable key wrapping and vetted node-local AEAD. Local scratch/journal/cache encryption, nonce/rewrap/revocation and real hardware dispatch are part of the contract.
- Linux x86_64/aarch64 are release targets. The AI workflow uses isolated worktrees, bounded build resources, progressive retrieval/LSP, deterministic gates and capped independent review rounds.
- JEV selected research/plan/plan-validation scope for this deliverable; its packaging choice was uncertain, so the author selected linked topic documents and a single task ledger. This is an automated implementation decision, not a human approval.

## Validation

Initial results are recorded in [planning validation evidence](evidence/planning-validation.txt), followed by the historical [first cost-extension validation](evidence/cost-planning-validation.txt). The immediate research results and final structural checks are linked above. They validate documentation/task constraints, arithmetic and the stated synthetic/model bounds. They are not product tests, cloud benchmarks, independent architecture reviews or proof of runtime security/cost properties.

## Next implementation action

Use `python docs/plan/tools/plan.py next` to select dependency-ready tasks. Begin M01 workspace/bootstrap and harness, then run the engine and storage spikes in M02/M03 as their task dependencies allow. Set up Git hosting/authorization through the actual chosen repository before claiming protected CI or external PRs exist.

The main open risks are K01 remote engine inputs, K03 provider conditional semantics, K04 pin/GC coordination, K07 durable consensus integration, K08-K10 crypto/cache lifecycle, and K18 exact required OpenSearch API coverage. Real cloud credentials/fleet/hardware/spend are not available as evidence in this planning task; M15/M17 cannot be marked done without them.

## Updating progress during development

1. Claim one ready task with an owner session, base commit, path/resource ownership and expected acceptance evidence. Set its status to `in_progress` in `tasks.json` through the integrator.
2. Record a concise checkpoint at meaningful boundaries: objective, constraints, decisions, changed files, observed validation, unresolved issues, next action. Keep large logs in immutable run artifacts referenced by a small local evidence record.
3. Set `review` only when the artifact is concrete and tests/coverage are available. Bind reviewer findings/verdicts to the actual final tree and risk-specific contract.
4. Set `done` only after its definition of done and dependencies are satisfied; add evidence paths and precise results. A failure or NOT-RUN remains incomplete. For `blocked`, record blocker and next action; do not infer blocked solely from elapsed time.
5. Update affected milestone rollup, regenerate pages, run plan checks and include the ledger/progress change with the implementation integration. Keep requirements and decision register aligned when scope changes.

Use role owners as allocation hints; they are not claims that a person or agent has already accepted work. Planned future PR/publish/deploy actions still follow the user's authorization and actual environment rules.
