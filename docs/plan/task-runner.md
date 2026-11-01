# Owned worktree tasks

Status: M01.03 implementation in qualification. The following interface is implemented locally; live recovery, independent review and hosted checks remain required before integration.

The coordinator runs its trusted `tools/task.py` or `cargo xtask` entrypoint from the integration checkout. Workers receive a packet with exact base SHA, goal, paths, assumptions, positive/negative acceptance, allowed commands, owner session, byte budget and expected evidence. `harness/task-packet.schema.json` defines the versioned format. Packets stay below 24,000 UTF-8 bytes and context responses label their bytes/4 token estimate; semantic routing and measured token budgets remain M01.04.

```sh
cargo xtask task packet M01.03 --owner <session> --paths tools/task.py \
  --negative 'Overlapping claims must refuse' > packet.json
cargo xtask task claim packet.json
cargo xtask context <claim> --owner <session>
cargo xtask task run <claim> prefetch --owner <session>
cargo xtask task run <claim> check --owner <session>
cargo xtask evidence show <claim> --owner <session>
cargo xtask task resume <claim> --owner <session>
cargo xtask worktree cleanup <claim> --owner <session>
```

`--state-root` and `--resource-state-root` may appear before or after the command group. All tasks in a coordinator bind the same resource registry. Use the existing host-wide sandbox registry rather than creating a pool per worker. The coordinator registry is namespaced by the canonical Git common directory. Worktrees and branches have collision-safe task IDs; active path claims reject equal/ancestor paths, including case aliases. Readiness is checked from the committed packet base. The harness lifecycle never marks the product ledger done; the integrator still owns acceptance, review and ledger updates.

Mutable Cargo/target and private RustFS resources remain task-specific. A frozen copy of the coordinator's monitor/CA/probe scripts lives outside worker source and is mounted read-only. Worker edits cannot replace these controllers. Kernel CPU/memory/swap/PID limits, actual-volume disk admission, sampled disk/deadline checks and bounded child output remain required. Repository children cannot reopen the monitor's descriptors through `/proc`; failure to establish this isolation prevents launch. Acknowledged container IDs are persisted before start, with bounded local logs. No Docker socket or production credentials enter the workload container.

Evidence uses immutable content-addressed JSON plus an atomic, locked registry. Context verifies referenced content and fetches only recent metadata; raw logs stay in artifacts. Every check binds before/after source identity, including committed head, dirty diff and untracked inputs. Unknown/ignored inputs, out-of-claim changes, symlink aliases and source changes during validation refuse. Output truncation or a known nonzero workload cannot be a passing result.

After a worker exits, resume checks its PID creation identity and actual container state. A live/unknown worker or running container prevents duplicate execution. Only an acknowledged immutable container ID may be reconciled; deletion uses that exact owned ID without force, so a queued start cannot recreate it. Recovery records interruption and requires checks to rerun. An unacknowledged create or uncertain setup/worktree lifecycle retains its claim for inspection. A stale timestamp is never reclamation evidence.

Cleanup validates repository/branch/path identity, ownership, clean tracked/untracked/ignored state, live handles and resource leases. It uses normal Git worktree removal and exact owned Docker cleanup. Branches and evidence remain. No global prune, forced worktree removal, branch deletion or recursive shell deletion is used. These controls coordinate trusted development sessions; full coordinator/agent-loss evaluation and the frozen corpus remain M01.08.
