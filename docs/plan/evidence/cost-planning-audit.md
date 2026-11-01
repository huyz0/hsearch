# Cost research and plan-extension audit

Historical snapshot: M00.06 first introduced the cost extension, including an M18 workstream. The user's subsequent instruction to research now supersedes that packaging and the payload-plus-head-CAS default. See [immediate research audit](cost-research-now-audit.md), [current decisions](../research/cost-decisions.md) and [retired ID mapping](../milestones/M18.md). The table below records the earlier state, not active task ownership.

Prepared 1 November 2026 (Australia/Sydney). This record covers the additional user requirement to minimize S3/API and cross-AZ cost with explicit latency/reliability tradeoffs. Runtime cost claims remain unverified until their implementation/real-provider tasks pass.

| Requirement | Updated evidence | Acceptance in the plan |
| --- | --- | --- |
| Research actual request and network cost drivers | Cost economics source-backed analysis and expanded source register | Distinguishes regional object access, cross-zone peers, NAT/LB/endpoints, KMS and provider-specific units/rates |
| Avoid hidden per-shard API floor | Architecture commit-domain revision and M18.03 | Payload plus root/page/key requests and failure-safe domain promotion measured before metadata freeze |
| Make tradeoffs selectable | Operating profiles and R23 | Economy/resilient object and HA/zonal quorum contracts expose changed latency, capacity, RPO/RTO and availability |
| Prevent silent reliability downgrade | CostPolicy module contract, risks D15/K22, M18.05 | Eligibility precedes cost ranking; budget pressure cannot weaken accepted quorum/freshness/security/retention |
| Cost-specific milestones and release gating | M18 plus dependencies/DoD updates in existing milestones | Early profile/protocol gates, locality/meters, Pareto qualification, real-provider usage reconciliation and M17 release gate |
| Concrete reviewable arithmetic | cost_model.py and cost-scenarios.json | Explicit counts, endpoint rates, units and omissions; arithmetic and bad-input controls, not a fake price quote |

Author review checked the cost dependency additions for deadlocks, preserved existing milestone/task IDs, and kept product work planned. The initial single-shard authority protocol remains a correctness reference; grouped compatible domains are a required measured/proved design decision, not asserted implemented behavior. Cross-tenant key sharing and pre-commit visibility shortcuts were rejected.

The final observed structural and arithmetic results are recorded in [cost validation evidence](cost-planning-validation.txt). Existing initial planning evidence is preserved as history. No cloud API workload, bill, network-path probe, independent AI review or runtime latency/failure experiment was executed for this extension.
