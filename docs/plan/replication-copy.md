# Cross-cluster replication and copy modules

Status: proposed, required delivery in M10-M12. CCR and copy are independently enabled modules using public snapshot/export/change-stream contracts. Their correctness does not depend on ordinary query workers having the modules installed.

## Product boundaries

| Operation | Result | Ongoing source dependency |
| --- | --- | --- |
| Snapshot/PIT | Fixed view of declared shard checkpoints | Retains source roots while pin lives |
| Same-store clone, optional | New logical index sharing immutable objects | Shared object/key/GC roots; disclose these dependencies |
| Cross-cluster copy | New independent destination at a declared source snapshot cut | None after completed verification and source-pin release |
| CCR | Read-only follower continuously applying source commits | Source stream and retention until explicitly detached/promoted |

Replication is initially active-passive, follower initiated, matching the operational shape of [OpenSearch CCR](https://docs.opensearch.org/latest/tuning-your-cluster/replication-plugin/index/). It is asynchronous across regions. Source acknowledgement does not imply remote durability. Active-active conflict resolution and synchronous inter-region consensus require another architecture decision; they are not hidden feature flags on CCR.

## Shared export and import substrate

An export descriptor includes source deployment/index/incarnation, schema/analyzer versions, supported engine/object/cipher formats, topology generation, per-shard committed/base/searchable checkpoints, immutable file inventory and digests, snapshot pin/expiry, and authorized read method. It contains no source long-lived credentials or plaintext keys. A source snapshot barrier guarantees a consistent per-shard cut and all its dependencies; it does not claim a global multi-shard transaction absent such a protocol.

Destination staging has a new index UUID, crypto domain, local job identity, byte/time budgets, import policy and expected source identity. Incomplete staging is hidden from aliases/search. A resumable job records verified object chunks, durable per-shard import coordinates, source pin lease, retries and errors. The checkpoint advances only after destination storage and import metadata are durable. “Bytes sent” is never a destination durability receipt.

Default transfer decrypts on an authorized source/export worker, sends plaintext only through authenticated encrypted transport, then re-encrypts into fresh destination objects/DEKs under destination policy. A destination-side worker may instead decrypt source ciphertext with a narrow KMS grant if residency/permissions allow it. Either path authenticates source bytes before use, streams with bounded memory and records provenance. Do not forward encrypted provider-SSE internals as a portable object format.

An opaque ciphertext optimization requires equal approved crypto domain, suite, format and authorization, plus a proved independent destination envelope and storage graph. It is disabled across tenants/domains by default. KEK rewrap cannot make ciphertext with source-tenant AAD valid in a new tenant domain. Never claim that changing a key name in metadata performs cross-domain re-encryption.

## One-time copy state machine

`requested -> authorizing -> snapshotting -> transferring -> verifying -> activating -> complete`, with `paused`, `failed`, `cancelling`, and `cancelled` states. Every transition is conditional and restart-safe.

1. Validate source export and destination create permissions, endpoint allowlists, quotas, supported formats, destination name collision policy, key availability and expected source incarnation.
2. Obtain a pinned snapshot cut. If a source quorum tail is not archived, materialize/archive that exact committed cut before transfer, or transfer the pinned tail with the same proof. A segment inventory that omits the acknowledged tail is not a complete copy.
3. Copy manifests/schema and immutable files in bounded parallel chunks. Persist integrity-verified progress and renew the source pin. Retries use immutable transfer identities. Lost pins stop transfer and trigger an explicit new snapshot decision, not silent mixing of cuts.
4. Verify all destination dependencies, lengths/hashes/AEAD, document IDs and versions, counts, sampled queries and a full logical-history digest where available. Run destination-only open and recovery with source network/KMS credentials blocked.
5. Atomically activate the destination descriptor and optional alias after successful verification. Concurrent copies with the same request identity resolve to one job/outcome; other name collisions fail unless an explicit replace workflow was selected.
6. Release source pin and staging metadata. Cancellation never deletes active unrelated destination data. Partial objects enter a scoped delayed cleanup queue. A completed copy reports cut/checkpoints, data bytes, key policy, and independent-read verification evidence.

Changing shard count or incompatible engine format uses a logical export/reindex mode, not a physical copy with edited metadata. Preserve source document identity and declared versions where supported, and expose deliberate version remapping. Copy must support RustFS-to-RustFS in separate buckets/deployments before clouds; same-bucket prefix copy alone does not prove independence.

## Continuous replication protocol

1. Create a follower binding to one immutable source index/incarnation and a replication epoch. Source grants read of snapshot/change stream; destination grants local import only. Followers reject public writes, including bulk and aliases that could bypass the block.
2. Bootstrap via the copy substrate at source checkpoint vector `C`. Establish retention for `C+1` **before** the snapshot can race log GC. A single coordinated export lease covers base and tail; registering a lease after copying is too late.
3. Pull bounded batches of committed **resolved operations**, ordered per source shard, carrying source identity/sequence, digest, schema/analyzer generation and deterministic result. Deliver at least once, apply idempotently. A duplicated identity with different content is corruption. A gap stops advancement and requests replay; never fill it with a newer checkpoint.
4. Make imported operations durable through the destination's active log/authority before acknowledging follower durable progress. Persist source-to-local coordinate mapping and source outcome identity atomically with application metadata. Local sequence IDs may differ; source identity remains stable for deduplication and failover analysis.
5. Apply and refresh independently. Report lag in bytes, operations, time, durable/applied/searchable positions and last successful contact. Schema/alias/topology events required to interpret mutations precede them. Do not replicate cluster credentials, node membership, unrelated admin settings or arbitrary scripts.
6. Follow source split/merge topology events with barrier cuts. Old-stream retention remains until the follower crosses the transition and pins new roots. Reject unsupported source format before acknowledging it; schema incompatibility is a paused error requiring migration.

Retention leases carry expiry and byte/time caps. Source protects its local service if a stalled follower exceeds budget: pause/error the follower and require rebootstrap, or deliberately reject source writes only in a separately selected strict DR policy. The default must not retain unbounded history. A stalled consumer cannot override another tenant's disk budget. [OpenSearch's retention-expiry recovery](https://docs.opensearch.org/latest/tuning-your-cluster/replication-plugin/force-resume/) is a useful precedent for making this explicit.

Pause stops pulling while retaining a bounded lease if authorized; resume verifies the source still retains the next required operation. Stop/detach makes the follower independent only after sealing/recording its cut and changing its write policy. A deleted/recreated source name is a different UUID and cannot be followed automatically without an explicit new binding. Auto-follow rules have separate allowlists, quota limits and audit.

## Promotion, failback and disaster recovery

Planned promotion: fence source writes, obtain its final committed token, wait for follower durable and applied/searchable barriers as requested, verify independent destination object/key recovery, then atomically activate the destination's new writer epoch and update routing. Record promotion evidence and expected old-source rejection. Once new destination writes exist, failback is another copy/replication migration, not reopening the old leader at its old epoch.

Unplanned promotion: measure the last verified replicated coordinate and declare the lost/unavailable suffix as a possible RPO gap. A partitioned remote cluster cannot prove the source is dead by timeout. Require an external fencing mechanism or explicit disaster promotion authorization that acknowledges the split-brain risk. The system must not advertise automatic zero-RPO failover from asynchronous CCR.

Cross-region journal replicas are not CCR. Local three-zone quorum protects one failure domain model; a remote follower protects a different disaster scenario with measurable lag. A copy is not an ongoing DR replica. Each status response identifies the contract in use.

## Required failures and operational hooks

Tests cover source/destination kill at every checkpoint, dropped response after durable import, duplicate/out-of-order/gapped batches, expired pins/leases, source recreation, destination name collisions, schema upgrades, key revoke/rotate, cross-tenant grants, certificate rotation, slow destination, disk full, long partition, partial copy cleanup, topology split during catch-up, and GC racing bootstrap/promotion. Run two separate clusters with separate storage and KMS test scopes. M15 adds genuine cloud/provider combinations and region-sized network delay.

Expose structured status, pause/resume/cancel, retry diagnosis, checkpoint diff, lease inventory and per-phase cost. Read-only AI tools can inspect those records. Mutation tools require scoped operational authorization and audit; diagnostics cannot silently promote a follower or extend a lease forever. Module removal refuses active jobs/retention participants unless explicitly drained or migrated.
