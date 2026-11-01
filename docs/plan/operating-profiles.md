# Cost, latency and reliability operating profiles

Status: proposed implementation contract with [research decisions completed now](research/cost-decisions.md), not implemented options. Select a profile explicitly at deployment/index activation; expose it in capabilities, receipts, query responses, diagnostics and cost reports. The selected production baseline is `object-resilient`, with a 250 ms maximum batching wait as an initial benchmark setting. `object-economy` starts at 1 s and accepts compute-zone recovery delay; `latency-ha` pays for a persistent cross-zone journal. These are batching settings, not total latency promises. M02 calibrates numerical workload targets and M15 qualifies real cloud cost.

## Named presets over explicit policies

| Profile | Commit and persistent data | Compute and traffic | What is traded |
| --- | --- | --- | --- |
| `object-economy` | Acknowledge after the next authorized fenced regional WAL slot is conditionally created; checkpoints are amortized | One active compute cell; disposable local cache; direct same-region object reads, no mandatory peer payload mirror | Lower idle compute/traffic, potentially slower batching/cold queries and service rebuild after cell loss. Regional committed data remains durable within the qualified store's failure model |
| `object-resilient` | Same regional object commit contract | Multiple zone-local cells with measured N-1 capacity and bounded warm state; independent object catch-up; cross-zone fallback only to an eligible reader | More compute/cache and metadata/read requests for better service availability. Avoids routine full payload replication between compute zones, but does not promise zero total cross-zone/network cost |
| `latency-ha` | Majority fsync to three full voters across three zones; async regional object archive | Durable voter volumes; selective fresh query replicas; zone-local queries where eligible | Lower write latency with unavoidable cross-AZ journal bytes and operational overhead. One-zone loss preserves acknowledged commits; loss of all unarchived voters exposes the declared archive-lag suffix |
| `latency-zonal` | Majority fsync to full voters in one zone; async regional object archive | Same-zone journal/serving workers; no regional quorum guarantee | Explicit optional lower-resilience deployment. Zone loss can lose every unarchived acknowledged record and service until recovery. It cannot be labeled `latency-ha` or selected automatically under cost pressure |

Three nodes in one zone are not three failure domains. Two full voters plus a metadata-only witness are not interchangeable with three full replicas. A regional bucket's durability does not imply that an unarchived local journal suffix is regional. RustFS uses the actual configured disk/node/rack/zone topology; its S3 API alone confers no multi-AZ durability.

Single-zone storage classes or an ephemeral acknowledgement path are not initial presets. S3 Express or another zonal service may be evaluated as a derived cache with exact API/cost/zone-loss tests. Introducing it as authoritative storage requires a new failure-model profile and migration review.

## Independent policy fields

Persist a versioned policy descriptor containing:

- Commit backend and failure domain: object store class/region/topology or journal voter placement, replica count and quorum contract.
- Batch controls: maximum wait, encoded bytes, dirty-stream cohort and maximum per-request deadline; manifest/checkpoint publication policy.
- Visibility: `published`, `at_least(token)` or `latest`, refresh policy, declared staleness bound for a bounded published-read policy and explicit partial-result permission.
- Placement and warm state: preferred zone/cell, eligible fallback zones, active-engine/cache caps, minimum healthy capacity and optional prewarm budget.
- Cost controls: request/byte envelopes, background bandwidth/IOPS, cohort/pack bounds, retry cap, KMS cache limits and forecast thresholds.
- Archive/replication: maximum age/byte exposure, hard disk reserve, CCR retention/lag budget and independent DR requirements.
- Security/data handling: tenant/crypto/residency domain, approved providers/classes/paths and policy freshness. No cost profile disables mTLS, AEAD authentication or tenant isolation.

Initial batch experiments should sweep 10 ms, 50 ms, 250 ms, 1 s and 5 s plus byte triggers where the profile's contract allows them. These are experiment inputs, not a blanket 5-second default. Economy can accept a larger object-commit budget; latency-ha still pays quorum/fsync, refresh and queueing even if object archival is batched for seconds. Freeze separate p99/availability/RPO/RTO and cost envelopes for each supported profile in M02.07. The existing 25 ms quorum/1 s object targets are reference targets, not universal guarantees across these modes.

## Topology-aware selection

The coordinator prefers a local-zone eligible query replica, then compares permitted alternatives by estimated latency and marginal request/byte cost. Eligibility enforces authorization, supported formats, activation and checkpoint **before** cost ranking. If a cheaper reader is stale, a strong request waits/routes to a valid reader or fails its freshness deadline. A cost optimizer cannot change `latest` to `published`, omit a shard or return partial results without the request policy allowing it.

For cache fills, first coalesce local misses, then consider same-zone peers versus an exact-version object range. Cross-zone peers require a measured byte/request/latency comparison. Leaf search returns bounded candidate metadata/aggregates; full document fetch is delayed until required final hits. Compare this with an eager-fetch baseline, including the remaining fetch hop. Affinity works through ingress, coordinator, worker and service endpoints; fixing just one hop can leave a cross-zone hairpin.

Zone-local routing is a preference with an explicit fallback policy, not an availability boundary by accident. Failure tests must move traffic to surviving cells, measure cache misses/rehydration and request spend, and verify N-1 capacity. Prewarming every tenant in every zone is forbidden by default; warm only justified working sets with a budget.

## Profile and authority transitions

Validate profiles before readiness: required storage semantics/classes, voter failure domains, key policy, budgets, resource reserves and supported read mode must match. Refuse conflicting settings such as `latency-ha` with all voters in one AZ or a zonal-only WAL claiming regional RPO=0. Report the actual physical topology and policy generation rather than trust a profile name.

Changing batch/prefetch/background limits may be an online policy update if it cannot extend a deadline or weaken any already accepted operation. A new profile that changes commit authority, durability/failure domain, key domain, storage class or guaranteed visibility requires explicit authorization, fencing/barriers and the existing mode-migration protocol. Already returned receipts retain their original meaning.

Budget pressure may reduce optional prefetch, pause bounded compaction/copy/CCR work within retention limits, rate-limit new work or reject it before acceptance. It must not reduce quorum, extend archive exposure beyond the hard cap, expire live pins, serve stale strong reads, retain revoked keys or drop acknowledged mutations. Long-delayed maintenance can increase cost and recovery risk; each background task has a maximum debt/age policy and a visible typed reason when admission is blocked.

Auto-tuning is limited to the declared allowed range and records before/after policy generation, observed load/cost signal and constraint checks. No automatic reliability downgrade. Crossing a planned spend threshold produces a bounded diagnostic/approval-ready impact report; it does not itself create permission for destructive changes or purchases.

## Profile acceptance matrix

Every advertised profile must run the same logical correctness/security corpus with its own declared failure cases, then a matched workload cost/latency sweep. Report successful/rejected load, request attempts by kind, encoded bytes by directed path, cache duplication, KMS calls, idle/background/recovery spend and price-card version. Compare profiles only after identifying changed semantics and resource capacity.

`object-economy`: kill its compute zone, verify zero loss of object-committed receipts and measure service reactivation/RTO and rebuild cost. It cannot inherit `object-resilient`'s availability target without the surviving capacity.

`object-resilient`: lose one cell/zone, verify existing strong-read semantics and recovery of committed data, measure traffic/cost spike and demonstrate surviving capacity. Test metadata/KMS/auth dependencies as well as workers.

`latency-ha`: lose one voter/zone and preserve all acknowledged operations; meter normal two-remote-follower traffic, catch-up and archive lag. A large batch reducing PUT count cannot count as a cross-zone byte reduction unless compression/content transmission actually changed.

`latency-zonal`: explicitly demonstrate the declared possibility of losing unarchived receipts after complete zone loss, while an archived cut restores correctly. A fault test reporting that exposure is a successful test of the stated limited contract, not a passing HA/RPO=0 result. This profile remains opt-in and its risk must be visible before activation.

M00 records the completed protocol/source research and 72 batching experiments now. M02 freezes workload-specific numerical envelopes; M03/M06 implement and prove the selected fenced/compatible-domain economics; M07 proves locality; M08 exposes evidence; M09 proves journal contracts; M15 runs actual provider cost/usage reconciliation; M16 verifies operator workflows. M17 gates all advertised profiles. There is no separate late cost milestone. [Research and cost equations](research/cost-economics.md) provide the evidence and [performance/scale](performance-scale.md) retains the service benchmark protocol.
