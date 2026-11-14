# M02.10 evidence: openraft per-group cost and failure schedules

`provisional`: 4-CPU sandbox VM, release build, tokio with 2 workers, in-process channel
network (no serialisation, no sockets), in-memory log and state machine, no snapshots. One run
of `experiments/openraft-groups` (raw output below). Relative, not absolute, and it measures
the library's overhead, **not durability**: nothing here touches an object store.

Setup: 3 members per group, `openraft` 0.10.0 + `openraft-memstore` 0.10.0, heartbeat 250 ms,
election timeout 600-1200 ms. Groups are initialised through node 1, so the "election" in the
scaling table is an immediate takeover, not a contested election; contested failover is in the
schedules.

## Per-group cost (idle groups, then one group written to)

| groups | RSS MiB | KiB per group | idle CPU (% of one core) | idle RPC/s | write p50 ms | write p99 ms |
|---|---|---|---|---|---|---|
| 1 | 31 | n/a (runtime baseline) | 0.8 | 8 | 0.08 | 0.20 |
| 10 | 35 | 391 | 3.4 | 73 | 0.06 | 0.22 |
| 100 | 76 | 409 | 14.6 | 727 | 0.06 | 0.24 |
| 300 | 195 | 394 | 31.0 | 2,154 | 0.07 | 0.17 |
| 600 | 430 | 385 | 55.6 | 4,290 | 0.07 | 0.26 |

Reading: about 390 KiB resident per 3-member group (all three members counted, in one
process) and about 7 idle RPC/s per group, which is the two followers' heartbeats at 250 ms
(8/s) as configured. Idle CPU grows linearly, about 0.09% of a core per group here with a
zero-cost in-process network; a real network and serialisation add to it.

Arithmetic, not measurement: at the project target of 1M tenants x about 50 indexes, one quorum
group per index would be 50M groups, about 19 TiB of resident group state and about 350M idle
heartbeat RPC/s at these settings. A group per index is therefore not viable on this evidence
even before durability cost; any quorum use would have to be per shard-set or per node, with
heartbeats amortised. This is an estimate from linear extrapolation of 600 groups, and the
scaling at orders of magnitude beyond that was not measured.

## Schedules (one group of 3)

| | no delay | 5 ms one-way delay |
|---|---|---|
| steady commit p50 / p99 | 0.06 / 0.12 ms | 12.7 / 13.9 ms |
| cut the leader: first leader elsewhere (poll, includes the poller) | 2,232 ms | 1,907 ms |
| first write on the new leader | 0.3 ms | 6.5 ms |
| in-flight write on the cut leader | client timeout, not applied on any member after heal | same |
| members agree after heal | yes (last_applied 204/204/204) | yes |
| 10% message loss: 100 writes | 100 ok, p50 0.2 ms, p99 208 ms | 100 ok, p50 12.7 ms, p99 220 ms |
| 30% message loss: 100 writes | 100 ok, p50 28 ms, p99 1,405 ms | 100 ok, p50 99.6 ms, p99 614 ms |

Notes and limits:
- Commit latency at 5 ms one-way delay is about 2.5 round trips of the delay, so a PUT-backed
  append at the measured 4-5 ms floor would sit in the same range per quorum round.
- The failover figure is one observation per setting, not a distribution; the 600-1200 ms
  election window plus lease expiry puts it near 1.9-2.2 s here. Do not read it as a bound.
- The in-flight write that timed out at the client was **discarded** by the new leader's log:
  "client timeout" did not mean "committed". That is one run of one schedule; other timings
  could commit it, so a client must treat a timeout as unknown and resolve by identity.
- Loss is a seeded xorshift over a shared counter, deterministic in this schedule but not a
  model of real network loss. Only drops and delays were injected: no duplication, reordering,
  asymmetric partitions, restarts with log loss, or disk faults.

## Not done
- No raft-rs run: it is a no-I/O library and needs an application harness for storage, network
  and scheduling, so a comparable number would measure that harness. Contract comparison is in
  [m02-10-quorum-candidates.md](m02-10-quorum-candidates.md).
- No object-store log under either library, no multi-process or real-network run, no restart
  or snapshot schedule, no CPU/RAM/disk/PID envelope beyond the RSS cap.
- The archived M02.10 governance (review checkpoints, controls) was not run; M02.10 stays
  `in_progress`. No runtime authority is selected here; that is M02.04.

## Raw output
```
cpus: 4, tokio workers: 2, RSS ceiling 1536 MiB

## Group scaling (3 members per group, in-memory log, no network delay)

| groups | RSS MiB | KiB/group | idle CPU % of 1 core | RPC/s idle | leaders in | write p50 ms | write p99 ms |
|---|---|---|---|---|---|---|---|
| 1 | 31 | 28188 | 0.8 | 8 | 0.0s (all groups) | 0.08 | 0.20 |
| 10 | 35 | 391 | 3.4 | 73 | 0.0s (all groups) | 0.06 | 0.22 |
| 100 | 76 | 409 | 14.6 | 727 | 0.0s (all groups) | 0.06 | 0.24 |
| 300 | 195 | 394 | 31.0 | 2154 | 0.1s (all groups) | 0.07 | 0.17 |
| 600 | 430 | 385 | 55.6 | 4290 | 0.2s (all groups) | 0.07 | 0.26 |

## Failure and ambiguity schedules (one group of 3)

### one-way RPC delay 0 ms: leader elected in 11 ms

steady: commit p50 0.06 ms, p99 0.12 ms (200 writes)

partition leader: in-flight write on the cut leader -> client timeout (outcome unknown to the client); new leader after 2232 ms ; first write on new leader took 0.3 ms
after heal: last_applied per member [Some(204), Some(204), Some(204)]; the in-flight write (client amb) applied on members [false, false, false] (true = it was applied)

loss 10%: 100 ok, 0 error or timeout of 100; p50 0.2 ms p99 208.0 ms; RPCs 1108 dropped 52
loss 30%: 100 ok, 0 error or timeout of 100; p50 28.0 ms p99 1405.1 ms; RPCs 1526 dropped 179

### one-way RPC delay 5 ms: leader elected in 12 ms

steady: commit p50 12.69 ms, p99 13.85 ms (200 writes)

partition leader: in-flight write on the cut leader -> client timeout (outcome unknown to the client); new leader after 1907 ms ; first write on new leader took 6.5 ms
after heal: last_applied per member [Some(204), Some(204), Some(204)]; the in-flight write (client amb) applied on members [false, false, false] (true = it was applied)

loss 10%: 100 ok, 0 error or timeout of 100; p50 12.7 ms p99 220.0 ms; RPCs 1148 dropped 51
loss 30%: 100 ok, 0 error or timeout of 100; p50 99.6 ms p99 614.2 ms; RPCs 1587 dropped 183

```
