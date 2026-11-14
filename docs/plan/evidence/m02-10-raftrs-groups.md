# M02.10 evidence: raft-rs per-group cost and schedules

`provisional`: 4-CPU sandbox VM, release build, **one driver thread**, in-process message queue,
`MemStorage` (no disk), no network, no serialisation. `experiments/raftrs-groups`, raw output at
the end. raft-rs does no I/O, so the harness (the application's tick/ready loop) is part of what
is measured, and this is **not** an apples-to-apples comparison with
[m02-10-openraft-groups.md](m02-10-openraft-groups.md): openraft there runs async tasks, channels
and a tokio runtime per group on two worker threads, raft-rs here runs a plain loop. It does not
measure durability.

Setup: 3 members per group, 50 ms ticks, heartbeat every 5 ticks (250 ms), election timeout
12-24 ticks (600-1200 ms), matching the openraft run. Each row is a fresh process. Groups start
with no leader, so the election column is a real contested election, unlike the openraft table.

## Per-group cost (idle groups, then one group written to)

| groups | RSS MiB | KiB per group over an empty runtime | idle CPU (% of one core) | idle requests/s | contested election s | commit p50 ms | commit p99 ms |
|---|---|---|---|---|---|---|---|
| 1 | 2 | n/a (baseline) | 0.0 | 8 | 0.9 | 0.005 | 0.034 |
| 10 | 3 | 41 (granularity) | 0.0 | 80 | 0.9 | 0.005 | 0.041 |
| 100 | 3 | 13 | 0.4 | 796 | 1.0 | 0.005 | 0.028 |
| 300 | 5 | 11 | 0.6 | 2,377 | 1.8 | 0.005 | 0.045 |
| 600 | 8 | 11 | 1.4 | 4,714 | 1.7 | 0.005 | 0.031 |

Reading: about 11 KiB resident per 3-member group at 300-600 groups (RSS is reported in whole
MiB, so the low rows are coarse) and about 7.9 idle requests/s per group, the same heartbeat rate
as openraft: that rate is set by the heartbeat interval and the group count, not by the library.
Idle CPU is about 0.002% of a core per group in this single-threaded loop.

Arithmetic, not measurement: 50M groups at 11 KiB is about 525 GiB resident, and about 395M idle
heartbeat requests/s. So raft-rs's per-group memory is roughly 40x smaller than the openraft
run's (about 440 KiB), and group memory alone is no longer the obvious blocker for a group-per-index. The 11 KiB is a
residency floor (in-memory storage, near-empty log, no snapshots, no network buffers, no durable
log), not a full per-group cost, and the figures are not like-for-like with openraft's. The idle
heartbeat rate is about the same and is what a group per index cannot afford without coalescing
heartbeats per node pair; neither library exposed such coalescing in the APIs read for this
experiment (not checked against their source or issue trackers).
Linear extrapolation from 600 groups; larger counts were not run.

## Schedules (one group of 3)

| | in-process, no delay |
|---|---|
| contested election | 602 ms |
| steady commit p50 / p99 | 0.005 / 0.067 ms |
| cut the leader: new leader elsewhere (50 ms tick granularity) | 953 ms |
| first write on the new leader | committed |
| write proposed on the cut leader | applied on no member after heal |
| members agree after heal | yes (commit index 203/203/203) |

Notes and limits:
- One observation per schedule. Failover near 0.95 s sits inside the 600-1200 ms election window
  plus a tick; it is not a bound. A proposal made on the cut leader was **not applied anywhere
  after the heal**: the new leader's log won (inferred: the proposal was accepted on the cut leader, but its later truncation was not observed directly). That shows what happened once, in this timing; a
  client must still treat a timeout as unknown.
- No message loss, delay, duplication, reordering or asymmetric partition schedule was run for
  raft-rs (openraft got loss and delay): only a clean cut and heal.
- Commit latency is the in-process cost of the loop (a few microseconds) and says nothing about
  a network or a durable log.

## Not done
- No object-store log, no real network, no restarts or snapshots, no KMS (moved to M05.08), no
  disk or PID envelope beyond the process RSS.
- No comparison on identical drivers: the openraft and raft-rs harnesses differ.

## Raw output
```
| groups | RSS MiB | (RSS - empty-runtime RSS) KiB/group | idle CPU % of 1 core | idle request/s | contested election s | commit p50 ms | commit p99 ms |
| 1 | 2 | 388 | 0.0 | 8 | 0.9 | 0.005 | 0.034 |
| 10 | 3 | 41 | 0.0 | 80 | 0.9 | 0.005 | 0.041 |
| 100 | 3 | 13 | 0.4 | 796 | 1.0 | 0.005 | 0.028 |
| 300 | 5 | 11 | 0.6 | 2377 | 1.8 | 0.005 | 0.045 |
| 600 | 8 | 11 | 1.4 | 4714 | 1.7 | 0.005 | 0.031 |

## raft-rs schedules (one group of 3, in-process, 50 ms ticks)

contested election: leader after 602 ms
steady: commit p50 0.005 ms, p99 0.067 ms (200 proposals, no network delay)

cut leader: new leader after 953 ms; first write on it committed
after heal: the write proposed on the cut leader was applied on members [false, false, false]; commit index per member [203, 203, 203]
messages sent 1688 (requests 851) dropped 12 (cumulative)
```
