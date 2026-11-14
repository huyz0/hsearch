# M02.10 evidence: quorum-candidate contract comparison (source reading only)

Status: **read from published source, nothing executed.** No per-group resource was observed and no failure schedule was run against either candidate. Numbers elsewhere in this task (RustFS request/CAS/batching) are in [m02-bench-profile-bulk.md](m02-bench-profile-bulk.md) and are `provisional`. Nothing here selects a runtime authority; that is M02.04.

Sources, fetched from crates.io and read only (not built):

| Candidate | Version | Published | Licence | Edition |
|---|---|---|---|---|
| `openraft` | 0.10.0 | 2026-11-10 | MIT OR Apache-2.0 | 2024 |
| `raft` (tikv/raft-rs) | 0.7.0 | 2023-03-07 | Apache-2.0 | 2021 |

## Contracts as the source states them

| Question | openraft 0.10.0 | raft-rs 0.7.0 |
|---|---|---|
| Shape | Async runtime-driven library; the application supplies storage, network and a state machine as traits. | Synchronous library with no I/O: the application drives a `RawNode` `tick` / `step` / `ready` / `advance` loop. |
| Persistence | `RaftLogStorage`: `append(entries, callback: IOFlushed)` acknowledges durability through a callback; `save_vote`, `truncate_after`, `purge`, `get_log_state`. | `Storage` trait read side (`initial_state`, `entries`, `term`, `first_index`); the application persists what each `Ready` hands it and calls `advance`/`on_persist_ready`. |
| Network | `RaftNetworkV2`: `append_entries`, `vote`, `pre_vote`, `full_snapshot`, `transfer_leader`, optional `stream_append`, `backoff`. | Application moves `Message`s itself; the library defines none of the transport. |
| Membership | Joint consensus, documented under `docs::cluster_control`. | Joint consensus via `propose_conf_change`, `ConfChange`/`confchange.rs`, `ConfState`. |
| Many groups | The crate is one group per `Raft`; many groups are the application's job (not verified for shared scheduling or timers). | Same: a `RawNode` per group, the application schedules them. |
| Runtime coupling | Async runtime abstraction with a tokio implementation behind a feature. | None: no runtime dependency. |

## What this means for an object-store-only design (not decided)
- Both candidates assume each member keeps a **local durable log** and talks to peers over a network. Neither has an object-store log. Using either under "the blob store is the only durable tier" would mean writing the storage trait against object storage, and an `append` acknowledged only after a PUT inherits the measured 4-5 ms floor per PUT and the corrected contended-CAS ceiling of roughly 80-100 commits/s per hot key (and its identity-resolution cost) in [m02-bench-profile-bulk.md](m02-bench-profile-bulk.md).
- openraft's callback-based append makes a batched PUT natural to express; raft-rs's `Ready` loop does too, at the cost of the application owning all scheduling and the network.
- Ambiguous outcomes: the object seam already resolves a lost PUT response by identity (see [m03-02-rustfs-probe.md](m03-02-rustfs-probe.md)); neither library models that, so each would need the storage layer to hide it.

## Not done, and why
- No ambiguity/failure schedules against either candidate and no per-group CPU/RAM/disk/PID observations: that needs building and running them, which was not done in this pass.
- M02.10 stays `in_progress`.
- Version choice is by newest published; a pinned-rev comparison is still owed.
- The matched sparse/busy/skewed comparison is in [m02-bench-profile-bulk.md](m02-bench-profile-bulk.md); KMS path accounting moved to M05.08.
