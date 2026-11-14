# M02.10 evidence: openraft per-group cost and failure schedules

`provisional`: 4-CPU sandbox VM, release build, tokio with 2 workers, in-process channel
network (no serialisation, no sockets), in-memory log and state machine, no snapshots.
`experiments/openraft-groups` (`scale <n>` per row, then `schedules`), raw output at the end. Relative, not absolute, and it measures
the library's overhead, **not durability**: nothing here touches an object store.

Setup: 3 members per group, `openraft` 0.10.0 + `openraft-memstore` 0.10.0, heartbeat 250 ms,
election timeout 600-1200 ms. Groups are initialised through node 1, so the "leaders" step in
the scaling table is an immediate takeover, not a contested election; contested failover is in
the schedules.

## Per-group cost (idle groups, then one group written to)

Each row is a fresh process, so no row inherits another's retained heap (an earlier single-process
sweep understated per-group memory this way and was replaced).

| groups | RSS MiB | KiB per group over an empty runtime (31 MiB) | idle CPU (% of one core) | idle RPC/s | write p50 ms | write p99 ms |
|---|---|---|---|---|---|---|
| 1 | 31 | n/a (baseline) | 0.4 | 7 | 0.06 | 0.15 |
| 10 | 35 | 3,218 (runtime growth dominates) | 2.8 | 73 | 0.06 | 0.14 |
| 100 | 74 | 719 | 13.0 | 731 | 0.06 | 0.22 |
| 300 | 160 | 534 | 27.8 | 2,181 | 0.06 | 0.18 |
| 600 | 290 | 489 | 49.6 | 4,315 | 0.07 | 0.19 |

Reading: the marginal cost between 100 and 600 groups is (290 - 74) MiB / 500 = about 440 KiB
per 3-member group (all three members, one process); the average over the first groups is higher
(up to about 720 KiB at 100). Idle RPC is about 7.2/s per group: two followers' heartbeats at the
configured 250 ms (8/s) minus startup. Idle CPU is about 0.08% of a core per group with a
zero-cost in-process network; a real network and serialisation add to it. The 1-group CPU figure
is within the 5 s window's quantisation (about 0.2%).

Arithmetic, not measurement: at the project target of 1M tenants x about 50 indexes, one quorum
group per index is 50M groups: about 21 TiB resident at the marginal 440 KiB (more at the
average) and about 360M idle heartbeat RPC/s at these settings. A group per index is therefore
not viable on this evidence even before durability cost; any quorum use would have to be per
shard-set or per node with amortised heartbeats. Linear extrapolation from 600 groups; behaviour
orders of magnitude beyond that was not measured.

## Schedules (one group of 3)

The injected delay is added to each request; the response is not delayed, so an exchange costs the
delay once, not twice.

| | no added delay | 5 ms added per request |
|---|---|---|
| steady commit p50 / p99 | 0.06 / 0.15 ms | 12.6 / 12.9 ms |
| cut the leader: first leader elsewhere (polled, 10 ms granularity) | 2,270 ms | 1,961 ms |
| first write accepted on the new leader (after retries) | 0.3 ms | 6.7 ms |
| in-flight write on the cut leader | client timeout; never applied on any member after heal | same |
| members agree after heal | yes (last_applied 204/204/204) | yes |
| 10% message loss, 100 writes | 100 ok, p50 0.1 ms, p99 201.5 ms | 100 ok, p50 12.7 ms, p99 222.8 ms |
| 30% message loss, 100 writes | 100 ok, p50 2.5 ms, p99 1,604.8 ms | 100 ok, p50 13.0 ms, p99 1,416.7 ms |

Notes and limits:
- A commit at 5 ms added per request took about 12.6 ms, about 2.5 times the added delay (a quorum
  round plus the leader's own append and apply). No object-store PUT was measured here, so this
  says nothing yet about a PUT-backed append; that comparison would need the storage layer built.
- Failover is one observation per setting, not a distribution. It exceeds the 1,200 ms election
  maximum; lease expiry is a likely contributor but that was not measured, and the no-delay run
  being slower than the delayed one shows run-to-run noise. Do not read it as a bound.
- The write that timed out at the client was **never applied on any member** after the heal. That
  does not show it was never appended to the cut leader's log (the entry may have been queued
  but not appended), and a different timing could commit it, so a client must treat a timeout as
  unknown and resolve by identity.
- Loss is a seeded xorshift advanced atomically; which RPC draws which value depends on task
  scheduling, so loss patterns are not reproducible run to run, and the RPC/dropped counters in
  the raw output are cumulative across phases, not per-row loss rates. Only drops and delays were
  injected: no duplication, reordering, asymmetric partitions, restarts with log loss or disk
  faults. The RSS cap is checked after a row, not during it; there is no wall-clock bound beyond
  the fixed polls.

## Not done
- raft-rs was run separately with its own harness: [m02-10-raftrs-groups.md](m02-10-raftrs-groups.md). The two are not like-for-like drivers, and raft-rs's per-group memory there is about 40x smaller; the per-group figure here includes openraft's async runtime tasks and channels.
- No object-store path accounting under raft (KMS moved to M05.08); M02.10 stays
  `in_progress`. No runtime authority is selected here; that is M02.04.

## Raw output
```
## Group scaling (3 members per group, in-memory log, no network delay; one fresh process per row)
| 1 | 31 | 28332 | 0.4 | 7 | 0.0s (all groups) | 0.06 | 0.15 |
| 10 | 35 | 3218 | 2.8 | 73 | 0.0s (all groups) | 0.06 | 0.14 |
| 100 | 74 | 719 | 13.0 | 731 | 0.0s (all groups) | 0.06 | 0.22 |
| 300 | 160 | 534 | 27.8 | 2181 | 0.1s (all groups) | 0.06 | 0.18 |
| 600 | 290 | 489 | 49.6 | 4315 | 0.2s (all groups) | 0.07 | 0.19 |

(rows above from fresh-process runs; the schedules below are from the final run after retrying new-leader writes)
cpus: 4, tokio workers: 2, RSS ceiling 1536 MiB


## Failure and ambiguity schedules (one group of 3)

### 0 ms added to each request (response undelayed): leader elected in 11 ms

steady: commit p50 0.06 ms, p99 0.15 ms (200 writes)

partition leader: in-flight write on the cut leader -> client timeout (outcome unknown to the client); new leader after 2270 ms ; first write on new leader took 0.3 ms
after heal: last_applied per member [Some(204), Some(204), Some(204)]; the in-flight write (client amb) applied on members [false, false, false] (true = it was applied)

loss 10%: 100 ok, 0 error or timeout of 100; p50 0.1 ms p99 201.5 ms; RPCs 1175 dropped 58
loss 30%: 100 ok, 0 error or timeout of 100; p50 2.5 ms p99 1604.8 ms; RPCs 1618 dropped 192

### 5 ms added to each request (response undelayed): leader elected in 11 ms

steady: commit p50 12.55 ms, p99 12.92 ms (200 writes)

partition leader: in-flight write on the cut leader -> client timeout (outcome unknown to the client); new leader after 1961 ms ; first write on new leader took 6.7 ms
after heal: last_applied per member [Some(204), Some(204), Some(204)]; the in-flight write (client amb) applied on members [false, false, false] (true = it was applied)

loss 10%: 100 ok, 0 error or timeout of 100; p50 12.7 ms p99 222.8 ms; RPCs 1107 dropped 50
loss 30%: 100 ok, 0 error or timeout of 100; p50 13.0 ms p99 1416.7 ms; RPCs 1546 dropped 182

```
