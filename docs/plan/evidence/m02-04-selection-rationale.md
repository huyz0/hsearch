# M02.04 selection rationale: durability and metadata candidates

Status: draft from the M02.10 evidence, which is itself not closed. M02.04 stays planned: see "What is not established". Every number is `provisional` (4-CPU sandbox VM, RustFS 1.0.4 in Docker, in-process Raft harnesses) and relative only.

## Decision

1. **No custom consensus protocol.** If a quorum is ever needed, it is an existing library (openraft 0.10.0 or raft-rs 0.7.0), pinned. Neither is selected here; the contracts are compared in [m02-10-quorum-candidates.md](m02-10-quorum-candidates.md).
2. **A Raft group per index is not viable as a default.** The idle heartbeat load is set by group count and heartbeat interval, not by the library: about 7-8 requests per second per 3-member group in both harnesses, so 50M groups (1M tenants x 50 indexes) would be about 4x10^8 idle requests a second before any write (arithmetic from the raft-rs run, not measured) ([openraft](m02-10-openraft-groups.md), [raft-rs](m02-10-raftrs-groups.md)). Memory differs by library (about 440-490 KiB marginal for openraft with its async tasks, about 11 KiB resident floor for raft-rs with in-memory storage and no network buffers), but the heartbeat rate does not. Any quorum design must coalesce heartbeats per node pair or avoid per-index groups; neither library's APIs read here exposed that for the application (not checked against their source or issue trackers).
3. **Object-store compare-and-set is the leading candidate for the first-stage commit primitive, not selected**: an uncontended commit on distinct keys is one PUT at any concurrency, but contended fencing is unproven (see below) and the hot-key cost is high.

## Costs (from [m02-bench-profile-bulk.md](m02-bench-profile-bulk.md), raw output included)

- A PUT has a fixed cost of about 4-5 ms before bytes matter: 16 KiB moves about 3 MiB/s, 4-16 MiB about 90-100 MiB/s. Batching documents into megabyte-scale PUTs is what makes request count and cost tolerable.
- Uncontended commit: 1.00 PUT per commit for sparse (16/s) and busy (352/s, 16 writers on their own keys).
- One hot key: about 50-100 commits/s regardless of writer count; with 90% of attempts on one key a commit costs about 1.19 PUT, 1.25 HEAD and 0.19 LIST, throughput falls to 58/s and p99 to 3.7 s. A LIST is priced like a PUT. Cost is driven by hot keys, not writer count. Design consequence: shard the head per index or coalesce writers per head, never let many writers race on one head.

## Ambiguous outcomes and failure limits

- Under the hot key the adapter's unknown-outcome resolution (list and probe recent versions within a bound) sometimes runs out and returns `ResolutionUnavailable` (the skewed matched cell had 14 'other errors' in 5 s and the 16-writer contended cell 20; the one variant recorded is process-wide, so the split per row is not attributed). A commit protocol must treat that as unknown and be idempotent on retry; this is parked in [the plan README](../README.md).
- An earlier contended-CAS result was invalid (identical bodies). The likely cause is that `If-Match` compares a content-hash ETag, so an update rewriting identical bytes is not fenced. **Not probed.** Until it is, a commit must carry a unique per-commit value in the body or key and never rely on the ETag of unchanged content alone.
- Both Raft harnesses showed a write proposed on a cut leader applied on no member after the heal (one observation each; raft-rs's truncation was inferred, not observed). A client that times out, or proposes on a node it cannot confirm is leader, must still treat the outcome as unknown (the raft-rs harness has no client timeout; it only proposed on the cut leader).
- Failover after a leader cut took about 0.7-0.95 s (raft-rs, 600-1200 ms election window, 50 ms ticks) and about 2.0-2.3 s (openraft, in-process); one observation per run, not bounds.

## What is not established

- **No open-loop batching sweep.** The size sweep is closed-loop with one writer; M02.04's validation asks for an open-loop one.
- **No KMS number.** KMS path accounting needs a real provider and is M05.08, which comes after M02. M02.04's validation still lists KMS accounting, so M02.04 cannot close on this evidence unless that line is waived or moved to M05.08 by an explicit plan change; the M02.04 and M02.07 envelopes must be revisited when M05.08 reports.
- No object-WAL prototype, no object-store-backed Raft log, no real network, restarts or loss schedule for raft-rs, no real-provider or multi-AZ cost.
- ARM: the manual `qualify (manual)` workflow (suite `arm64`) passed on commit e4e02bc, a correctness run; the performance numbers here are x86_64 sandbox only.
