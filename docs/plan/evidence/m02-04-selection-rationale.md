# M02.04 selection rationale: durability and metadata candidates

Status: see "What is not established". Every number is `provisional` (4-CPU sandbox VM, RustFS 1.0.4 in Docker, in-process Raft harnesses) and relative only.

## Decision

1. **No custom consensus protocol.** If a quorum is ever needed, it is an existing library (openraft 0.10.0 or raft-rs 0.7.0), pinned. Neither is selected here; the contracts are compared in [m02-10-quorum-candidates.md](m02-10-quorum-candidates.md).
2. **A Raft group per index is not viable as a default.** The idle heartbeat load is set by group count and heartbeat interval, not by the library: about 7-8 requests per second per 3-member group in both harnesses, so 50M groups (1M tenants x 50 indexes) would be about 4x10^8 idle requests a second before any write (arithmetic from the raft-rs run, not measured) ([openraft](m02-10-openraft-groups.md), [raft-rs](m02-10-raftrs-groups.md)). Memory differs by library (about 440-490 KiB marginal for openraft with its async tasks, about 11 KiB resident floor for raft-rs with in-memory storage and no network buffers), but the heartbeat rate does not. Any quorum design must coalesce heartbeats per node pair or avoid per-index groups; neither library's APIs read here exposed that for the application (not checked against their source or issue trackers).
3. **Object-store compare-and-set is the leading candidate for the first-stage commit primitive, not selected**: an uncontended commit on distinct keys is one PUT at any concurrency, but contended fencing is unproven (see below) and the hot-key cost is high.

## Costs (from [m02-bench-profile-bulk.md](m02-bench-profile-bulk.md), raw output included)

- A PUT has a fixed cost of about 4.8 ms before bytes matter: 16 KiB moves about 3.2 MiB/s, 4-16 MiB about 96-105 MiB/s. Batching documents into megabyte-scale PUTs is what makes request count and cost tolerable.
- Uncontended commit: 1.00 PUT per commit for sparse (16/s) and busy (352/s, 16 writers on their own keys).
- One hot key: about 78-92 commits/s from 2 to 16 writers (50-85/s at one writer); with 90% of attempts on one key a commit costs about 1.2-1.3 PUT, 1.3-1.4 HEAD and 0.2-0.3 LIST (two runs), throughput falls to about 59/s and p99 to about 3.6 s. A LIST is priced like a PUT. Cost is driven by hot keys, not writer count. Design consequence: shard the head per index or coalesce writers per head, never let many writers race on one head.

## Ambiguous outcomes and failure limits

- Under the hot key the adapter's unknown-outcome resolution (list and probe recent versions within a bound) sometimes runs out and returns `ResolutionUnavailable` (the skewed matched cell had 10-14 'other errors' in 5 s across runs and the 16-writer contended cell 20-27; the one variant recorded is process-wide, so the split per row is not attributed). A commit protocol must treat that as unknown and be idempotent on retry; this is parked in [the plan README](../README.md).
- An earlier contended-CAS result was invalid (identical bodies) because `If-Match` compares a content-hash ETag, so an update rewriting identical bytes is not fenced. **Probed and confirmed (14 November, RustFS 1.0.4):** an update with identical bytes from a stale version is accepted and 16 identical-body writers saw zero conflicts, so a commit must carry a unique per-commit value in the body or key and never rely on the ETag of unchanged content for fencing. Other S3 implementations are not tested.
- Both Raft harnesses showed a write proposed on a cut leader applied on no member after the heal (one observation each; raft-rs's truncation was inferred, not observed). A client that times out, or proposes on a node it cannot confirm is leader, must still treat the outcome as unknown (the raft-rs harness has no client timeout; it only proposed on the cut leader).
- Failover after a leader cut took about 0.7-0.95 s (raft-rs, 600-1200 ms election window, 50 ms ticks) and about 2.0-2.3 s (openraft, in-process); one observation per run, not bounds.

## Open-loop batching and what a batch buys

The open-loop sweep (raw output in the bench evidence) shows the object API's limit on this VM for small PUTs: about 710 a second (64 KiB at 800/s offered completes 715/s with p50 359 ms; at 1200/s p50 is 1.8 s). Large payloads showed no queueing up to the highest rate offered (about 120 MiB/s at 1 MiB and 4 MiB), so 120 MiB/s is a lower bound, not a knee. Below saturation a PUT costs about 7-12 ms p50 at 64 KiB, about 17-22 ms at 1 MiB and about 42-46 ms at 4 MiB. Batching to megabyte-scale PUTs moves the same bytes with about 16-64x fewer requests. The size to choose is a latency-versus-cost trade the commit protocol makes (visibility target under 500 ms p50 leaves room for batching windows of tens of milliseconds, not seconds).

## Quorum follower bytes (arithmetic, separate from object API batching)

If a Raft quorum were used, the leader sends each entry to every follower: a 3-member group sends 2x the entry bytes over the network, a 5-member group 4x, on top of one durable write per member. That is replication traffic between nodes, priced separately from object requests (which batching reduces); it is not measured here, and cross-AZ transfer pricing is not applied.

## Scope of the attempts counted

Payload PUTs and per-key compare-and-set heads are counted request by request (PUT/HEAD/GET/LIST per commit). No root or page object exists until the commit protocol is designed (M06), so the per-key head is the stand-in for the root and **page-path accounting is not done here; it belongs to that task**. The network path is the arithmetic above, not a byte estimate. KMS: M05.08. The KMS clause of M02.04's validation was moved to M05.08 at the coordinator's direction on 14 November; the root/page and byte-estimate limits above are open and stated, not removed.

## What is not established

- **KMS number**: needs a real provider and is M05.08 (comes after M02); M02.04's KMS clause points there.
- No object-WAL prototype, no object-store-backed Raft log, no real network, restarts or loss schedule for raft-rs, no real-provider or multi-AZ cost.
- ARM: the manual `qualify (manual)` workflow (suite `arm64`) is a correctness run; the performance numbers here are x86_64 sandbox only.
