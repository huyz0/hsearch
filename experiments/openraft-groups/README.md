# openraft-groups (M02.10 experiment)

Standalone crate, deliberately outside the hsearch workspace: its dependency tree
(`openraft` 0.10.0, `openraft-memstore` 0.10.0, tokio) never reaches the product build,
`cargo deny` or coverage. Not shipped, not a product dependency.

Run inside this directory: `cargo run --release -- scale <groups>` (one group count per process;
`MAX_RSS_MIB`, default 1536, is checked after the row) and `cargo run --release -- schedules`. Results and their limits: `docs/plan/evidence/m02-10-openraft-groups.md`.
