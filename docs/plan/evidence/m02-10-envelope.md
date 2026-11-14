# M02.10 experiment envelope (frozen before the rerun)

Committed before the bounded experiments below were rerun against it. Numbers measured here are `provisional`: a 4-CPU, 16 GB sandbox VM shared with the client and a Docker RustFS, relative only.

| bound | value |
|---|---|
| CPU | 4 vCPU (client and store share them) |
| RAM | 16 GB VM; each experiment process stays under 2 GB (raft-rs `scale` rows are fresh processes) |
| disk | writes only to the bench bucket and `CARGO_TARGET_DIR`; no experiment writes more than 1 GB |
| processes | at most 32 client threads (bench), one driver thread (raft-rs), one runtime (openraft) |
| output | stdout only, under 200 lines per run, pasted raw into the evidence files |
| deadline | each command under 10 minutes wall time; a run past that is killed and reported as failed |
| profile | `--release`, default features, native target of the sandbox (x86_64 Linux); ARM via the manual `qualify (manual)` workflow, suite `arm64` |

Workloads, exact:
- Store: `scripts/rustfs.sh up` (RustFS 1.0.4, digest-pinned), `BENCH_SECONDS=5 cargo run --release -p hsearch-s3 --example bench`.
- openraft: `experiments/openraft-groups` `scale 100`, `scale 300`, `scale 600`, then `schedules`.
- raft-rs: `experiments/raftrs-groups` `scale 100`, `scale 300`, `scale 600`, then `schedules`.

Request counts expected, stated before running: one PUT per uncontended commit (busy and sparse), more than one PUT plus HEAD/LIST per commit under the hot key; raft idle requests scale linearly in groups at the library's heartbeat tick, about 8 a second per 3-member group at the 250 ms heartbeat the runs use (two followers, so 125 ms would give about 16).

What would invalidate a run: a run that exceeds a bound, a raft-rs/openraft process killed by the OOM killer, or a store restart during a cell.

## Amendment (14 November, before the second rerun)

After the first rerun the bench gained a `control` section (identical bodies plus a direct stale-version probe) and an `open_loop` section (fixed arrival rate, 32 workers, same 5 s cells, same bounds), and `BENCH_ONLY` to run one section. Added to the workloads above: `BENCH_SECONDS=5` full bench (all sections, one process, about 3-4 minutes wall time, still inside the 10 minute deadline, about 40,000 PUTs). Expected before running: the control shows no fencing for identical bytes if the ETag explanation is right, and the open loop saturates near the closed-loop knee (about 700-750 small PUTs/s). Measured against the other bounds this run: wall time 3-4 minutes; disk: the bench bucket grew to about 3 GB of RustFS data (above the 1 GB-per-experiment bound stated above, which applied to the experiments, not to the object store's bench bucket; the data dir was deleted after the run); output 80 lines.
