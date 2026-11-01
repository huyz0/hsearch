# Cost research: object requests, topology and failure contracts

Added 1 November 2026 (Australia/Sydney). This extends the initial research in response to the explicit cost requirement. Findings are from provider documentation and system design sources. Arithmetic examples are sensitivity models, not a deployment quote, bill or measured hsearch performance result. The user's location does not select a deployment region; Sydney and other regions need their own price exports.

## Recommendation

Keep regional object-durable storage as the economical baseline, with zone-local query/indexing cells and no routine cross-AZ payload replication between those cells. Use a same-region S3 gateway endpoint where applicable. Offer a separately priced three-AZ quorum profile for lower write latency. Let users choose cache warmth, batch delay, archive lag and compute redundancy explicitly, while preserving the chosen commit/freshness contract.

The first cost extension left initial architecture decisions mixed with later verification in M18. That has been replaced. [Cost protocol research and experiments are completed now](cost-decisions.md) in M00; implementation gates live directly in M02/M03/M06/M07/M09 and provider qualification in M15/M16. M18 is retired. The selected fenced append path removes the former mandatory per-batch root CAS; the comparisons below retain that old request floor as a sensitivity baseline, not a production requirement.

## What the provider documentation establishes

| Evidence | Architectural consequence |
| --- | --- |
| [S3 pricing](https://aws.amazon.com/s3/pricing/) excludes same-region S3-to-AWS-service transfer from its transfer charges | Fetching immutable data from regional S3 is different from transferring it between EC2 peers. GET requests, compute, KMS and network middleboxes can still cost money |
| [S3 gateway endpoint](https://docs.aws.amazon.com/vpc/latest/privatelink/gateway-endpoints.html) has no additional endpoint charge | Prefer this route for supported same-region S3 traffic; interface endpoints, NAT, inter-region routes and external clients need separate accounting |
| [AWS VPC example](https://aws.amazon.com/vpc/pricing/) shows NAT processing charges even for otherwise free same-region S3 transfer | An incorrect route can dominate storage-request savings. Validate routes, endpoint policies and DNS configuration in infrastructure; DNS resolution alone cannot prove the packet path |
| [AWS's 2026 NLB analysis](https://aws.amazon.com/blogs/networking-and-content-delivery/optimizing-data-transfer-costs-when-using-aws-network-load-balancer/) charges a cross-zone edge at sender and receiver, and shows multiple crossings | Account for each directed physical edge and payer. Do not call the combined two-endpoint charge a round trip, then double it again. ALB/NLB/TGW/PrivateLink have distinct rules |
| [GCS storage pricing](https://cloud.google.com/storage/pricing) treats different zones of the same region as the same storage location for the documented transfer exemption; [GCP VPC pricing](https://cloud.google.com/vpc/pricing) charges inter-zone VM traffic | Preserve the object-store versus peer distinction on GCP; do not apply an AWS rate or endpoint recipe to it |
| [Azure zone documentation](https://learn.microsoft.com/en-us/azure/reliability/availability-zones-overview) currently says same-region inter-zone transfer has no charge | A universal nonzero cross-AZ rate would mis-rank Azure designs. Still account for storage operations, network appliances, cross-region traffic and latency |
| [S3 storage-class pricing](https://aws.amazon.com/s3/pricing/) includes request/retrieval, minimum object size/duration and monitoring terms | Tiny mutable manifests, WAL chunks and frequently rewritten objects need a different tier policy from old immutable segments |
| [S3 Express FAQ](https://aws.amazon.com/s3/faqs/) describes single-AZ placement and additional upload/retrieval byte fees | Cheaper requests or lower latency alone do not make it the lowest total cost or a multi-AZ durable WAL. Evaluate as optional derived cache first, with separate capabilities/pricing |

Rates and exceptions change. Source date, provider, region, storage class, path, currency, byte unit, discounts and endpoint type belong in the rate card. Published examples are not an up-to-date Sydney quote. RustFS can measure request and byte amplification; it cannot prove AWS/GCP/Azure billing or a cloud SLA.

## Request amplification is the first design constraint

Account separately for WAL payload creation, commit publication, manifest pages/root, key envelopes, schema/catalog updates, metadata checks, segment opens, ranged reads, compaction, pin renewal, GC listing/deletes, failed/conflicting attempts, retry probes, copy/CCR and recovery. A high-level `put()` with hidden SDK retries is several billable attempts. A conditional read returning no new bytes still invokes an API; do not model a cache validation as free merely because the response body is empty.

The relevant write model is:

```text
PUT-like attempts = WAL payload + authoritative commit root + changed manifest pages
                  + key envelopes + schema/catalog/pins + retries/recovery/maintenance
read attempts     = metadata checks + useful range reads + prefetch/opens
                  + duplicate fills + retries/recovery/maintenance
```

Under the selected fenced-slot protocol, an ordinary append has one WAL create and zero additional head/key-sidecar writes. The root/page/envelope terms still count structural checkpoints, activation, segment publication, rewrap and retries at their actual frequency.

Timer-driven work must be **dirty-only**. An idle index/shard/cohort emits zero data/commit writes and no per-index heartbeat. Catalog discovery, membership heartbeats and lease work must have a bounded cost based on active capacity. At sparse load, batch counts cannot exceed the number of nonempty arrival windows. The previous `max(bytes/B, producers/T)` approximation describes busy producers and is only a rough lower estimate when time and size flushes interact; it is not an accurate sparse-tenancy predictor.

For fixed windows and Poisson arrivals with rate `lambda` per stream, expected dirty windows/s is `(1-exp(-lambda*T))/T`. For the selected dirty timer that starts at the first arrival, the time-trigger-only rate is `lambda/(1+lambda*T)`; these are different policies. Neither is a complete burst/size/IO model. [The executed simulator](../tools/cost_research.py) exercises dirty-start timers and byte triggers now. Production accounting still counts actual attempts, retries and crypto partitions; storage pacing/queueing requires runtime measurement.

### Packing does not eliminate the metadata floor

If 10,000 logical shards are dirty once per second, packing their payload into fewer objects still leaves 10,000 conditional root updates/s under a strict one-head-per-shard-per-batch protocol. At an illustrative USD 0.005/1,000 PUT-like requests and a 30-day month, the root updates alone are USD 129,600/month. Reducing payload requests while omitting this root cost would produce a misleading plan.

The completed research compared the following grouping alternatives and additionally selected **fenced append slots rather than a per-batch root CAS**. M03.08 now implements and qualifies that selected protocol:

1. Dedicated per-shard authority: simplest correctness reference and suitable for hot shards. Measure the full per-commit/root/page cost.
2. A bounded commit domain grouping small shards/indexes **within one tenant and compatible crypto/residency/durability policy**: one ordered conditional-create slot commits a batch; separate structural CAS publishes amortized checkpoint maps. Shared ordering does not promise arbitrary cross-shard API transactions. Hot streams can be fenced and promoted to dedicated domains.
3. Shared multi-tenant physical packing: optional later experiment, with separately authenticated/encrypted extents and bounded reference/GC/key overhead. One tenant's BYOK/revocation cannot become another's key. Do not assume one DEK or one KMS call can cover unrelated tenants/policies. This is not the default format.

Grouping requires an explicit authority/membership migration proof, bounded metadata pages, fair scheduling and per-tenant limits. A shared metadata bottleneck, reintroduced per-shard pointer writes, excessive page rewrites or key envelopes can erase the saving. The selected path uses dirty compatible groups, conditional-create data slots and amortized checkpoints. If full runtime/provider conformance fails, record a design revision and retain the comparison protocol for diagnosis; do not silently substitute it and claim the selected cost envelope passed.

Candidate payload controls: batch across compatible dirty streams, byte-and-age-triggered grouping, adapt the active writer cohort to load, combine small Lucene files into independently readable packs, keep metadata hot, coalesce neighboring ranges and collapse concurrent cache fills. Measure a block/pack-size sweep rather than freeze one enormous object. Large packs increase rewrite amplification, GC retention, cancellation waste and cross-tenant coupling if misused.

## What existing systems contribute

[WarpStream's distributed metadata/cache design](https://www.warpstream.com/blog/minimizing-s3-api-costs-with-distributed-mmap) studies object-request economics by batching data from multiple logical partitions and managing the resulting metadata efficiently. The lesson transfers: optimize data **and discovery/publication** together. Its metadata service and log semantics are not a proof that hsearch can skip authoritative commit publication.

[turbopuffer's tradeoffs](https://turbopuffer.com/docs/tradeoffs) explicitly trade object-store write latency and metadata checks against a simpler durable architecture. Use its distinction between cold/warm work and throughput/latency; retain hsearch's separately defined strong-read and committed-reader contracts.

The user's [pstore batching research](https://github.com/huyz0/pstore/blob/6ddaf56dd0e34862561b63b173ac3de5cfb50fb4/docs/research/05-storage-engine/batching-and-visibility.md) identifies producer count and flush cadence as major cost terms. Its [AZ topology research](https://github.com/huyz0/pstore/blob/6ddaf56dd0e34862561b63b173ac3de5cfb50fb4/docs/research/04-cluster/az-topology.md) proposes zonal cells over regional object storage. Adopt those questions, but reject unproved shortcuts: warm caches are not universally free, endpoint DNS does not prove absence of NAT, and replicated memory cannot count as the committed durable prefix. Cache duplication and retained/recovery bytes need their own ledger.

[AWS request-cost guidance](https://aws.amazon.com/blogs/storage/analyzing-request-and-data-retrieval-charges-to-optimize-amazon-s3-cost/) and [small-object compaction guidance](https://aws.amazon.com/blogs/storage/optimizing-storage-costs-and-query-performance-by-compacting-small-objects/) support examining object count, request patterns and compaction together. Compaction is worthwhile only if future read/storage savings exceed its compute, I/O, request and retention cost over the expected object lifetime. Schedule it with a rate budget and delete/latency objectives, not solely to reach a chosen file size.

## Cross-AZ bytes: classify before optimizing

| Traffic class | Default direction |
| --- | --- |
| Query fanout, top-k/fetch and aggregations | Route within an eligible zone-local cell; reduce on leaf before returning, delay full document fetch until final hits |
| Immutable segment/cache fill | Read directly from same-region authoritative storage; same-zone peer cache is optional. Compare a remote-zone cache with an object range by estimated cost and deadline |
| Source ingest forwarding | Account for client/LB zone and mutation owner. Group work locally where authority permits; don't elect a writer through a cost heuristic |
| Quorum payload/votes/catch-up | Keep required three-zone voter placement for `latency-ha`; compress safe payloads before transport encryption and meter each remote edge |
| Fresh search subscriptions | Subscribe only assigned replicas; avoid broadcasting every write to every query node or zone. Object-durable replicas can catch up from the durable WAL |
| Membership/metadata hints | Hierarchical/batched relays; no full-fleet per-index notifications or data-plane correctness dependence on gossip |
| Rebalance/compaction/cache warming | Rate-limited, locality-aware and checkpointed; record cold miss/replay cost caused by placement churn |
| Copy/CCR/snapshot restore | Count source/destination operations, inter-region/provider egress, retries and destination re-encryption. A low steady-state bill cannot hide recovery cost |
| NAT/LB/endpoints/KMS/telemetry | Count actual paths, processed bytes, hourly charges and payers; don't infer them from one cloud transfer rate |

For three full WAL voters in different zones, at least two follower payload copies are normally transmitted for each leader-accepted record eventually replicated to both followers. Majority acknowledgement may wait for only one, but that does not eliminate the other transfer, retransmission or recovery. Read replicas add more bytes. A batching setting reduces requests, not the bytes of required full replicas. Any reduced-replica design changes the failure contract and needs a separate selected profile.

Zone-local cells still need enough surviving capacity and warm state for the promised availability. Three smaller cells with no N-1 reserve do not automatically survive a zone loss at the original SLO. Compute/caches, KMS/auth/metadata dependencies and failover traffic need measurement. Zero routine cross-zone **application payload** is a goal in object profiles, not a claim that all networking/control/monitoring/DR costs vanish.

## Quantitative comparisons and units

The executable [cost model](../tools/cost_model.py) takes explicit operation counts and directed traffic edges, so it does not invent cloud latency or universal batch rates. The [example rate card and scenarios](../cost-scenarios.json) use illustrative USD inputs and a declared `2^30`-byte billing unit. They exclude discounts/free tiers and separate modeled request/network components from complete TCO.

- At USD 0.0004/1,000 GETs, one avoided GET is USD 0.0000004. If a cross-AZ edge is charged USD 0.01 at each endpoint per billing GB, the transfer-only break-even is about 21 KiB for **one** avoided GET. For an N-request avoided read, scale that threshold by N and subtract extra peer/cache costs. A 64 KiB remote-zone cache hit can therefore cost more in transfer than one direct same-region Standard GET, even before cache compute.
- At 10 MiB/s of encoded input, two remote follower copies and USD 0.02 combined/GB, the modeled replication bytes cost USD 1,012.50 per 30-day month. This excludes client traffic, query replicas, protocol overhead, retry/catch-up and disks/compute. Compression changes encoded bytes; acknowledgement batching alone does not.
- The VPC pricing page's Ohio example uses USD 0.045/GB of NAT processing. At a modeled 1 GiB/s through that path, this is USD 116,640/month in processing before hourly charges. This is a rate-example sensitivity, not a Sydney quote or a reason to bypass required network controls.

Use explicit byte units for each SKU. GCP documents GiB; AWS examples frequently convert 1 TB to 1,024 GB. Import the actual billing unit rather than mixing decimal GB with GiB. Request/response bytes are separate edges; combined sender/receiver rates must not be charged a second time as return traffic. These cases are negative controls in the model.

## BYOK and storage classes can reverse apparent savings

[AWS KMS pricing](https://aws.amazon.com/kms/pricing/) has its own key and operation charges. Count application wrap/unwrap, key-cache misses, envelope writes, rotation/rewrap and copy independently of payload GET/PUT. Grouping physically adjacent data with different key domains does not remove those key operations. Retain bounded DEK caches and policy lifetimes; costs cannot justify stale revoked keys.

[S3 Bucket Keys](https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucket-key.html) reduce the S3 service's SSE-KMS requests. They do **not** eliminate calls hsearch makes to its application `KeyProvider`. If both application BYOK and SSE-KMS are used, meter both. Do not apply the advertised SSE saving twice or assume it extends to the application's envelope scheme.

Default hot WAL, heads, keys and mutable metadata to an immediately readable regional class with qualified semantics. Evaluate colder tiers only for sufficiently old immutable segments/backups with lifecycle, retrieval, minimum-size/duration, pin and rehydration accounting. A tier transition that violates PIT/recovery/freshness is rejected. Express One Zone remains a separately qualified optional cache/storage-class experiment; it is not the regional baseline simply because its request price is lower.

## Required decisions and verification

Before implementation freezes the format, record cost per committed batch and query by workload, including root/page/key requests and retry attempts. Freeze per-profile request/byte/cost envelopes in M02.07 from operator-supplied workloads and price snapshots. A budget is not invented from an arbitrary USD/month number.

The selection criterion is minimum measured TCO **subject to** declared p99 latency, freshness, RPO/RTO, availability, tenant/security and resource constraints. Plot/record the Pareto frontier for batch delay, grouping/packing, cache warmth and zone replication; mark dominated choices. Savings based on weaker semantics must identify that changed contract. M15.06 rejects a claimed saving if the matched-scope failure or coverage checks do not pass.

Required evidence includes dirty-only idle tests, metadata-floor comparisons, small-object/query-range sweeps, retry storms, zonal failure with cost/freshness-preserving fallback, three-zone replication accounting, cold rebuild/CCR/copy costs and KMS rotation. Reconcile HTTP-attempt/edge counters with real cloud usage/billing exports in M15.07 before calling a provider profile cost-qualified. Tests on RustFS produce counts and synthetic cost estimates only.
