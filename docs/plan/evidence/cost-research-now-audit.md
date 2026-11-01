# Immediate cost research and milestone migration audit

Prepared 1 November 2026 (Australia/Sydney). Scope: the user's correction that cost research must happen now because M18 is too late. This author audit is not an independent architecture review or product qualification.

## Research delivered now

The [decision report](../research/cost-decisions.md) inspects the pinned fork's fencing RFC, `WalStore` and `WalGroupCommitter`, SlateDB's manifest/WAL/GC RFCs, turbopuffer's author transcript and WarpStream's classic/delayed sequencing designs. [Sources S74-S82](../research/sources.md) record this additional primary evidence. Earlier cloud pricing/path research remains applicable with its explicit region/rate/unit limits.

The selected object WAL commits by successful conditional create of the next authorized slot in a fenced, serialized lane. It removes the former mandatory second head CAS. Inline initial wrapped DEKs avoid an additional key-object PUT, but still require application KMS wrapping. Compatible tenant/key batching, amortized dirty checkpoints, authoritative latest barriers and zone-local serving complete the selected design. Retained fences, activation/reopen, unknown outcomes and GC boundaries are explicit correctness obligations.

Executed [batching experiments and bounded model](cost-research-results.md) are reproducible through [cost_research.py](../tools/cost_research.py). The 72 scenarios cover three workloads, three seeds, two grouping policies and four wait caps. Busy 250 ms grouping reduces batch counts by about 14.3 times; sparse grouping by only about 6%. No storage pacing, network, crypto, sustainable throughput or end-to-end service latency is measured. The model's 23-state safe result and three unsafe counterexamples concern only the documented atomic-slot race.

## Preserved delivery obligations

| Former task | Current task | Acceptance owner |
| --- | --- | --- |
| M18.01 | M02.07 | Calibrate researched profiles, workload and numerical cost/service envelopes |
| M18.02 | M03.07 | Actual request attempts, paths, directed bytes and attribution |
| M18.03 | M03.08 | Full fenced-domain prototype, model, packing and recovery cost |
| M18.04 | M07.07 | Eligible zone-local serving and surviving capacity |
| M18.05 | M09.08 | Explicit profiles and budget controls preserving guarantees |
| M18.06 | M15.06 | Matched-scope cost/latency/reliability qualification |
| M18.07 | M15.07 | Actual cloud usage/billing reconciliation |
| M18.08 | M16.07 | Operator workflows and scoped AI cost diagnostics |

The [retired page](../milestones/M18.md) preserves all eight former task headings and points to current acceptance. M00.07 owns research completed now. The active ledger has 18 milestones and 118 tasks, preserving all 108 original task IDs and adding the prior cost-extension tasks plus M00.07. No active milestone/dependency is M18. M17 release keeps every cost obligation through explicit migrated task gates.

Early gates are substantive: M02.06 reviews calibrated cost choices before feasibility closure; M03.08 depends on actual RustFS semantics, metering and the full authority model; M03.05 cannot freeze metadata without that gate; M06.01 requires it before production WAL integration. Later cloud/operator tasks retain provider-access and billing evidence requirements. Missing access is not treated as a passing zero-cost result.

## Consistency and validation

Architecture, format layout, module receipt, encryption/rewrap, read-barrier, profile, performance, requirement and risk documents use the selected fenced-slot semantics. Historical audits retain the former protocol/counts with explicit supersession notices. Release audit scope includes R01-R23. Only planning work can be completed by this revision; all 111 product tasks remain planned.

The ledger makes the completion scope explicit: M00 uses its own source/model/plan acceptance and author audit. Product tasks still require independent final-tree review and applicable runtime gates. Completing this research does not satisfy the independent architecture reviews scheduled for implementation.

[Observed command results](cost-research-now-validation.txt) record reproducibility, graph/link/render checks, negative controls, arithmetic controls and the migration/requirement/source inventory audit. These checks establish internal consistency and the stated model results. They cannot certify real provider conditional semantics, full epoch/GC safety, production crypto, the engine bridge, service SLOs or bills. Those are early implementation and qualification gates, not deferred initial desk research.
