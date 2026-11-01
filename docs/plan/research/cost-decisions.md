# Cost architecture decisions made now

Research completed 1 November 2026 (Australia/Sydney), before runtime implementation. This replaces the suggestion that M18 would make the first cost architecture decisions later. The decisions below use direct source inspection, 72 seeded batching experiments and a bounded fencing model executed now. Product/provider conformance, measured service latency and billing remain implementation evidence, not unfinished desk research.

## Selected design

1. **Regional object durability is the default.** Use `object-resilient` with zone-local serving cells; `object-economy` reduces warm compute and accepts rebuild delay. Keep cross-zone quorum an explicit `latency-ha` option. No routine cross-zone full-payload mirroring in the object profiles, and no hidden reduction of consistency or security to save money.
2. **Use ordered, fenced append slots for the object WAL.** A successful conditional create of the next authorized slot is its commit point. Do not require a second mutable-head CAS on every batch. Keep structural ownership/activation and segment/checkpoint publication separate. The former payload-plus-CAS design remains a comparison/recovery-test oracle, not the production cost default.
3. **Batch small compatible streams together.** A domain groups indexes/shards within one tenant and compatible crypto/residency/durability policy, with an ordered writer. Promote hot streams or split hot shards instead of letting one tenant head serialize unlimited work. Unrelated tenants/keys do not share one DEK.
4. **Store the initial wrapped DEK with the immutable WAL object.** This avoids a mandatory separate key-object PUT per batch. A later rewrap creates a versioned overlay, verified and published through the metadata/pin protocol. Payload AAD excludes mutable wrapping-provider/version fields; object/domain/block identities remain authenticated. This is a format requirement for the crypto review, not permission to implement custom cryptography.
5. **Make checkpoints amortized and dirty-only.** A WAL receipt identifies its committed slot independently of a newer manifest. Checkpoints and immutable segment manifests bound replay/discovery; they are not written for an idle shard or every append. Meter their pages, retries, keys and retained fence/tail objects. A replay-tail byte/count bound can force checkpoint progress or reject new admission before exceeding its envelope.
6. **Pay for freshness explicitly.** `latest` obtains an authoritative writer/activation barrier and reads through that committed prefix. `at_least(token)` has a known minimum; published/PIT reads use their declared snapshot. A stale checkpoint alone cannot reveal the latest tail. Price barrier RPCs, metadata validation and tail reads, and allow only checkpoint-eligible local or remote replicas. A cheap stale read is not a substitute for a strong request.
7. **Route the whole request path by locality.** Keep ingress, coordinator, worker and cache reads in the preferred zone when eligible. Prefer direct same-region object reads over an expensive remote-zone peer when that satisfies the deadline. Fetch final documents after leaf reduction; avoid broadcasting updates to every query worker.
8. **Keep cost enforcement inside the selected contract.** Tune batching, optional prefetch and permitted background work; reject new work when hard resource/archive limits require it. Never change quorum, visibility, retained pins, key policy or a returned receipt under budget pressure.

Initial experiment settings for implementation: object-resilient starts with a **250 ms maximum batching wait**, economy with **1 s**, both dirty-only with a byte trigger. These are benchmark starting configurations, not promised end-to-end latency. The research rejects a universal 5 s batching default: sparse tenants pay almost the full wait for small savings. Quorum journal coalescing must be independently constrained by its latency target. Block/cache/pack sizes require real Lucene access traces; the write simulation cannot select them.

## Why change the WAL commit decision?

Direct inspection of the user's pinned `WalStore.appendOne`, `establish`, `fenceOlderTerms` and `WalGroupCommitter` shows a one-conditional-PUT append design with serialized slots and failure poisoning. The [fencing closure RFC](https://github.com/huyz0/OpenSearch/blob/ad254f6439dc4e93f5f00461b85a086e53a3d2da/serverless/rfc-fencing-closure.md) explains why a lease timeout or a listing-based cutoff alone fails. The [implementation](https://github.com/huyz0/OpenSearch/blob/ad254f6439dc4e93f5f00461b85a086e53a3d2da/serverless/shell/src/main/java/org/opensearch/serverless/store/WalStore.java) places fences in the same namespace/slot where an old writer would append.

The next append and the fence race on one atomic create-if-absent. If data wins, recovery includes it before the eventual fence. If the fence wins, that writer stops. The writer must have only one append in flight per lane, poison the lane after any conflict/ambiguous outcome, establish an own-term fence and confirm authority on activation, and retain the occupied fence/tail metadata needed to exclude delayed writes. A successful append can be acknowledged after a handover if it committed before the seal and is included in successor recovery; a timeout is **unknown**, not a definite refusal.

[SlateDB's current pluggable-WAL RFC](https://slatedb.io/rfcs/0030-pluggable-wal/) independently describes fencing the WAL and manifest separately, conditional WAL creates, serialized batch writes and manifest-based replay boundaries. It is a design precedent, not a dependency to add or proof of hsearch's adapter. Its [GC boundary RFC](https://slatedb.io/rfcs/0026-garbage-collector-boundary/) also shows why deleting old occupied names can let a delayed writer falsely succeed. Keep fences and an unambiguous tail locator; apply the full boundary/idempotency/pin proof before reclaiming older records.

The fork uses listings during activation/recovery. hsearch must count and bound those operations, retain a structural epoch inventory and a checkpoint/tail locator, and avoid listing on each append/query. A bounded cold-recovery listing is an explicitly charged fallback, not a zero-cost steady-state mechanism. No attempt is made here to claim the Java fork is a finished Rust library or that its stale introductory comments specify every current engine call order.

This changes **which upload is authoritative**, not the safety requirement. An arbitrary UUID object upload remains an orphan; only the next ordered slot of a correctly activated/fenced lane commits. Schema/key/format dependencies and prepared conditional results must already be interpretable before that slot is written. Manifest publication and mode/domain migration still require their own conditional authority checks.

### Alternatives considered

| Option | Judgment now |
| --- | --- |
| Payload plus head CAS per batch | Keep as a simple comparison protocol. It adds an object write and often another dependent round trip; not selected as the default |
| Fenced ordered append plus amortized checkpoints | Selected research direction. Saves the per-batch head CAS, at the cost of strict lane serialization, failure poisoning and explicit tail/read/recovery handling |
| Arbitrary parallel lanes without a sequencer | Rejected for conditional document mutation order. Parallel unrelated domains are allowed; one ordered domain cannot skip a failed slot |
| WarpStream-style batched metadata service | Useful alternative where a separate service is acceptable. Its [classic write path](https://docs.warpstream.com/warpstream/overview/architecture/write-path) orders through metadata, which must be counted and operated |
| Acknowledge before authoritative ordering, then sequence later | Rejected for the current search contract. [WarpStream Lightning](https://www.warpstream.com/blog/the-art-of-being-lazy-log-lower-latency-and-higher-availability-with-delayed-sequencing) documents changed offset ordering; that is not a free optimization for create/CAS/update/version semantics |
| turbopuffer's object plus manifest CAS | Valid alternative with a different metadata/read tradeoff. Its [CMU transcript](https://turbopuffer.com/blog/video-andy-pavlo-cmu) describes this explicitly. A vendor's protocol is not evidence that every hsearch batch needs the same extra request |

## Experiments executed now

The [reproducible script](../tools/cost_research.py) generated and ran three workloads, three seeds, two grouping policies and four maximum waits, for **72 experiments**. The [results](../evidence/cost-research-results.md) and [raw JSON](../evidence/cost-research-results.json) include all counts and assumptions.

Each trace is 30 seconds of synthetic arrivals with 1 KiB encoded records, a 4 MiB cap, 50 logical indexes per tenant and two distinct key policies per tenant. Sparse load is about 100 records/s across 10,000 indexes; busy is about 5,000/s across 1,000; skewed directs 80% to one tenant. Timers start with the first dirty record and drain without an artificial final early flush. The simulation assumes instantaneous storage/crypto; it measures batching wait and request groups, **not cloud throughput or acknowledgement latency**. In particular, 50 ms groups may outpace a real single-in-flight object lane and would incur additional queue/coalescing delay. No p99 service claim is inferred.

| Observation | Mean across three seeds | Decision |
| --- | --- | --- |
| Busy, 250 ms cap: per-shard versus tenant/key batches | 2,226.24 versus 155.82 batches/s, about 14.3x fewer groups | Group compatible small streams; this reduces data and per-object wrap operations together |
| Sparse, 250 ms cap: per-shard versus tenant/key | 100.51 versus 94.58 batches/s, about 6% fewer | Do not advertise busy-workload savings for sparse isolated tenants |
| Sparse tenant/key, 250 ms versus 5 s | 94.58 versus 46.31 batches/s, but p95 batch wait rises from 250 ms to 5 s | Large delay is an explicit economy choice, not a universal fix |
| Busy tenant/key, 250 ms versus 1 s | 155.82 versus 40.00 batches/s, p95 batch wait about 245 versus 958 ms | Offer a latency/cost dial while keeping durability unchanged |

For any fixed set of groups, the model charges `N + 2*sum(floor(domain_batches/64))` WAL/checkpoint PUTs for fenced append with inline wrapping, versus `2*N + checkpoint_PUTs` for payload-plus-CAS. The checkpoint cadence of 64 is a sensitivity input, not a frozen production value. A separate initial key object adds another N PUTs, erasing that one-request benefit; inline wrapping does **not** remove N KMS wrap calls. Structural activation is reported separately instead of falsely multiplying startup cost into every steady-state minute.

### Executed fence-model controls

The bounded model explores one old writer, one successor, three record identities and atomic creates. The serialized/poisoned/retained-fence case passed **23 reachable states**. Three deliberately broken variants produced counterexamples: two in-flight records allow an acknowledged record beyond a fence; continuing after failure skips a fence; deleting a fence lets a paused PUT be acknowledged outside the recovered prefix. These controls demonstrate that the model's checked invariant can fail.

This is a small model of the core append race. It does not prove full activation, same-term reopen, multiple epochs/owners, GC/checkpoint publication, provider linearizability, real retries or crypto. Those remain concrete implementation conformance tests in M03/M05/M06. Their location is early because they decide whether the selected direction can ship; they are not a postponed research milestone.

## Network, KMS and cache decisions

The earlier [cost research](cost-economics.md) already established direct same-region object access, provider-specific zone-transfer rules, endpoint/NAT costs and the approximately 21 KiB one-GET/cross-zone-transfer break-even under the explicit example rates. This pass keeps those decisions and adds the missing read-authority cost: a fenced log's strong read needs an actual barrier/tail prefix, not just a cached manifest.

No automatic full working-set replication/prewarm across every zone. Warm-state duplication, surviving N-1 capacity, source-writer/barrier placement and cold rebuild are explicit profile costs. At low QPS, recomputing from objects may beat a permanently warm redundant fleet; at high QPS, warmed local readers can win. Current evidence does not justify one fixed cache ratio or three fully duplicated caches for every tenant.

Application `KeyProvider` wraps remain metered even when wrapped keys are inline or S3 Bucket Keys are enabled. Key reuse across unrelated tenants, unlimited key caching, bypassing AEAD or delayed ordering is not an accepted saving. Restore/copy/CCR and rewrap retain destination/source key independence and their own operation/byte budgets.

## Work moved earlier

There is no late M18 workstream after this revision. Research decisions and these experiments are completed in **M00** now. Profile/workload calibration moves to M02; meters, domain/slot/fencing prototypes and cost acceptance move to M03/M06; locality acceptance moves to M07; budget-safe journal behavior to M09. M15 keeps actual cloud usage/bill reconciliation and M16 keeps the operator workflow. Those later tasks validate implemented paths, not decide the baseline architecture for the first time.

No new user input was needed for these design choices. Actual production workloads, region prices and provider/hardware access are still required to certify absolute latency, capacity or spend. The research makes those variables explicit and provides runnable evidence now without inventing a deployment quote.
