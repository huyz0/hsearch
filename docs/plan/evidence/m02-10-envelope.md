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

Request counts expected, stated before running: one PUT per uncontended commit (busy and sparse), more than one PUT plus HEAD/LIST per commit under the hot key; raft idle requests scale linearly in groups at the library's heartbeat tick, about 8 a second per group at a 125 ms heartbeat.

What would invalidate a run: a run that exceeds a bound, a raft-rs/openraft process killed by the OOM killer, or a store restart during a cell.
