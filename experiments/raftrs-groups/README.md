# raftrs-groups (M02.10 experiment)

Standalone crate, outside the hsearch workspace like `experiments/openraft-groups`: raft-rs
0.7.0 and its tree never reach the product build, `cargo deny` or coverage. Not shipped.

raft-rs does no I/O, so this crate is the application loop (tick, step, ready, persist, send,
advance) over `MemStorage` and an in-process queue, on one driver thread.

Run inside this directory: `cargo run --release -- scale <groups>` (one group count per process)
and `cargo run --release -- schedules`. Results and limits:
`docs/plan/evidence/m02-10-raftrs-groups.md`.
