# Source-bound context and Rust navigation

M01.04 extends the [task runner](task-runner.md). It is a development tool, not a search API. Existing `cargo xtask context CLAIM --owner SESSION` metadata output retains its M01.03 shape. Add `--request request.json` for the versioned router. Both forms accept `--state-root` and `--resource-state-root`; use the same shared resource registry as the task coordinator.

```json
{"schema_version":1,"operation":"outline","file":"crates/types/src/lib.rs","max_bytes":16384}
```

```sh
python3 tools/sandbox.py --state-root "$RESOURCE_STATE" image
cargo xtask context "$CLAIM" --owner "$SESSION" --state-root "$TASK_STATE" --resource-state-root "$RESOURCE_STATE" --request request.json
```

The `harness/context-request.schema.json` and operation-specific field checks reject unknown versions, arbitrary commands, write flags, unsafe paths and invalid bounds. Operations are `packet`, `search`, `slice`, `full`, `outline`, `definition`, `reference`, `rename-preview`, `diagnostics`, `compile`, `doctor`, `start`, `stop` and `stats`.

Start with a bounded literal `search` or a Rust `outline`, then semantic definition/reference queries and line slices. Search accepts a literal pattern, optional path, at most 50 items and an offset cursor. A nonzero search/reference cursor requires `expected_source` from the prior response. Source slices accept at most 200 lines. Individual retrieval files are bounded to 64 KiB; unsupported/oversized text units are explicitly listed as skipped in search results. Full-file retrieval requires a reason and a recorded narrower search/slice/outline attempt at the same source. A successful versioned response fits its requested budget (default 32 KiB, at most 128 KiB and half the task output budget); excess output refuses without publishing a successful context event.

Rust definition/reference/rename requests use an exact one-based line and a `find` string containing exactly one `<|>` cursor marker, for example `"line":4,"find":"pub fn <|>id"`. Rename also requires an identifier `new_name`. The adapter invokes the pinned CLI's exact `--scope LINE --find ... --project /workspace --output json` schema. Rename always omits `--apply` and requires `applied:false`; it returns a preview, never changes source. Returned location line numbers are one-based and characters are zero-based UTF-16, as reported by the pinned CLI. Outlines contain nested children. Source slices include file SHA-256 and line coordinates.

## Pinning and process ownership

`dev/navigation-tools.json` pins lsp-cli 0.4.1 at `cd9d492f950ab8fb8a9f54e4aea2f72cfd1aab48`, its canonical source lockfile hash and Rust 1.97.1's rust-analyzer/rust-src components. Startup verifies the installed lsp SHA-256, versions and runtime schemas; doctor records the actual resolved analyzer version/hash and schema/profile hashes. The inspected release executables require newer GLIBC than the pinned bookworm image. The image builder therefore fetches the exact source and compiles its locked dependencies in the existing capped non-root workload monitor, then copies only the bounded verified executable and provenance into the local development image. Building the Rust base/toolchain layer still uses the existing host Docker builder reservation. A source build does not promise a cross-host reproducible executable hash: each built artifact's actual hash is recorded and checked.

No redistribution license is declared in the inspected lsp-cli tree. Source and binaries are not vendored in hsearch. The development image is built locally or ephemerally in CI; this task does not publish an image, binary or release or infer a redistribution grant. Cached image identity covers Dockerfile, build-context policy, pin manifest, installer, frozen build monitor and image assembly controller. The source compile reuses the completed base build's 2 CPU/4 GiB/512 PID/8 GiB reservation without starting RustFS or stacking another build lease. Two 1 CPU/2 GiB navigation sessions fit the admitted capacity of the configured native CI runners. An exact immutable image ID is used after build.

Each worktree gets a distinct owned container, source mount, frozen controller snapshot, `LSP_CLI_HOME` and cargo/analysis volumes. Analysis is separate from the normal compile target/cache. A daemon holds a shared scheduler reservation of 1 CPU, 2 GiB RAM, zero swap, 256 PIDs and a 4 GiB sampled cache allowance. The guest verifies effective cgroup caps and actual backing-volume free space; this is monitored usage and reservation, not a filesystem quota. Queries and daemon logs have byte/deadline limits. Idle sessions expire after five minutes and all sessions after thirty minutes. Warm queries at the same source reuse the process. Explicit `start` replaces a stale/expired session; a normal stale query returns a labelled literal fallback rather than silently restarting it.

The service runs non-root, read-only, without capabilities, host sockets or host ports, and without network after a separate capped locked-dependency prefetch. Only task source is mounted read-only; mutable daemon/cache state is private. The source identity hashes actual task bytes/index/head before and after a request. These bookends detect observed changes; they are not an atomic filesystem snapshot. Do not edit a worktree concurrently with navigation or validation. Workspace build scripts and semantic output are untrusted development guidance; a process sharing a guest UID is not an independent integrity authority for its own output.

Lifecycle mutations serialize through the task claim. Acknowledged container IDs are persisted before inspection. Unacknowledged creates keep their reservation; unknown query outcomes keep a pending token and cannot be retried blindly. Stop acts only on verified owned IDs/volume labels, requests TERM, waits and removes without force; uncertain cleanup preserves its lease. Close navigation before worktree cleanup. Interrupted host workers require the runner's `resume` before new operations. Some partially acknowledged setup states require explicit inspection; there is no automatic age-based reclamation.

## Compiler and context evidence

Missing, stale or failed semantic queries return `semantic:false`, a reason and bounded lexical candidates. Such references are not resolved symbols; rename fallback is never an edit plan. Diagnostics always say `cargo_check_required:true`. `compile` first closes the semantic session, then uses the task runner's separate capped `cargo check --locked --offline --workspace --all-targets --all-features` command. The packet must allow `check` (or explicitly `compile`); prefetch the task's runtime dependencies first. Known compiler failure is returned as `passed:false` with retained command evidence and leaves the claim interrupted, requiring resume. Infrastructure failure is unavailable, never compiler success. Existing `check` retains the full workspace check contract.

Versioned request/response bodies are content-addressed task evidence. `response_payload_bytes` is the exact canonical UTF-8 JSON response size, including accounting/evidence fields, excluding its printed newline and transport framing. `stats` totals prior versioned router payloads only, excluding itself, the legacy metadata interface, other tools and model internals. `ceil(bytes/4)` is labelled as an estimate everywhere, with no identified tokenizer and no actual token-billing claim.

Run the real `tools/navigation_corpus.py` corpus after building the image:

```sh
python3 tools/navigation_corpus.py --state-root "$RESOURCE_STATE" --evidence-parent "$EVIDENCE_PARENT"
```

It needs capacity for two concurrent daemon budgets and retains new owned fixtures, full event transcripts and a report. Assertions use fixed source coordinates, declaration names and rename counts, not the implementation's output as its own oracle. It checks warm reuse, both live worktrees, cleanup isolation, stale fallback, a real type-error failure and a valid cargo check. Whole deterministic fixture costs include both roles, lifecycle/query payloads, compiler results and fixed review messages. The tiny Rust case can cost more than reading both small files; the larger controller-source retrieval case measures whether bounded retrieval helps. Neither substitutes for M01.08's frozen task/agent quality and total-cost trial or proves general productivity savings.
