# Independent review evidence gate

## Policy (current)

- **One independent reviewer by default** (`correctness_tests_claims`). A second role (`architecture_security`) is added only when changed paths are security-, format- or durability-sensitive (`tools/review.py` `SENSITIVE`: object/types crates, auth/crypto/TLS/secret/KMS, format/codec/wire, manifest/checkpoint/journal/WAL, commit/CAS). Routing is by path name, not by judgement.
- Round cap is unchanged: two planned rounds, hard ceiling three; at the ceiling, split or redesign.
- The reviewer never receives the author's reasoning. Empty findings are valid.
- On a hot, I/O or bounded-resource path the reviewer also checks the [resource and performance standard](engineering-standards.md#resource-and-performance-standard): bounds checked before allocation, nothing materialized that could stream, avoidable copies and allocations, request and byte cost counted, a recorded benchmark or profile, and stated downsides. A missing bound or an unmeasured performance claim is a blocking finding.

The rest of this page describes the packet/verdict protocol in detail and is reference, not required reading.

M01.05 turns the [review process](ai-harness.md#review-and-architecture-feedback) into a durable trusted-coordinator protocol. `tools/review.py` and `cargo xtask review` create bounded packets, ingest structured reports from assigned independent sessions, and refuse integration when reports, source identity, findings or required check evidence disagree. The gate does not authenticate a session, detect defects by itself, grant publication permission or submit formal GitHub reviews. The integrator must obtain the actual separate session reports and observed check results through authorized tools; a JSON file is not proof that its author or claims are truthful.

Use one explicit shared `--state-root` for all worktrees of the coordinator. State is namespaced by canonical Git common-directory identity, then keyed by task ID rather than mutable diff hashes. Reopening the store or changing a diff does not reset rounds. The counter charges issued packets, including incomplete rounds: two rounds are planned, three is the hard ceiling. At the ceiling, split/redesign rather than create a fourth packet or pass unresolved findings. Private local state is not an authentication boundary against a host user who can replace the entire store or choose a new one; the trusted integrator must preserve its recorded registry and history.

M02.10's [development addendum](archive/m02-10-development.md) permits separately admitted reversible source development and bounded ordinary synthetic controls after root audits a seed; it grants no candidate issuance or qualification before actual M02.11 integration. The same issued1/planned2/hard3 policy lineage is retained, with its prior R1 bound only to51187fb.

M02.10 alone has the prospective same-task completion exception in [M02.10 continuation](archive/m02-10-continuation.md): one additional candidate checkpoint, actual overall4/cumulative formal3/max4, after independently reviewed policy integration. All original BLOCKED history is retained; if this checkpoint remains blocked or incomplete, stop/split or redesign. This is not a global ceiling change or a fourth packet in any historical frozen gate. For that exception, a future ordinary current-repository formal1 must disclose cumulative formal3/actual overall4 and carry both exact-packet/source-bound legacy-resolution sidecars plus the coordinator's complete gate-time history audit; the local schema's hard_ceiling3 grants no additional cumulative round.

## Session adapter and workflow

Run cheap deterministic checks before issuing a packet. Freeze a committed clean worktree; actual source bytes, index, head, tree and repository identity are bound using the runner's checks, including hidden index flags and clean-filter controls. Do not edit that worktree while preparing, reviewing or gating it. These are observed source bookends, not atomic filesystem snapshots. Ignored files are refused; use the managed external build/cache volumes rather than a mutable ignored target directory in the source worktree.

Create `reviewers.json` from actual independent session identities. Sensitive paths (see Policy) require both roles; everything else needs the one correctness role. Roles have distinct assigned sessions and none may equal the implementer. Risk routing derives from actual original-base/head paths. A task retains its original base, every required check name and every review role across follow-up rounds, including after a code revert. Every current reviewer must be independent of all recorded task implementers. Reviewer adapters accept only the checked-in `harness/review-verdict.schema.json` format; there is no hard-coded model/provider, arbitrary shell launch or automatic user-identity approval.

```json
{"correctness_tests_claims":"separate-session-A","architecture_security":"separate-session-B"}
```

The trusted coordinator supplies observed checks as an array of objects with `name`, exact `head`, `conclusion:"success"`, and a concrete `evidence` reference. These inputs must come from the actual validators/hosted run, not a requested or inferred pass. Local checks can precede hosted CI; the final gate additionally always requires the current protected `ci-ok` check. GitHub branch protection and an expected-head merge guard remain independent server-side requirements; the local protocol does not bypass them.

```sh
cargo xtask review --state-root "$REVIEW_STATE" begin M01.05 --base "$BASE_SHA" --implementer "$IMPLEMENTER_SESSION" --reviewers reviewers.json --checks local-checks.json --reason "Initial review of the frozen implementation"
cargo xtask review --state-root "$REVIEW_STATE" record correctness-verdict.json
cargo xtask review --state-root "$REVIEW_STATE" record architecture-verdict.json
cargo xtask review --state-root "$REVIEW_STATE" gate M01.05 --checks final-local-and-hosted-checks.json
```

Send the generated packet to the actual assigned sessions and ask them to inspect the code, independent counterexamples and test/architecture/PR claims. The packet includes its source identities, changed paths, observed deterministic checks, required check names and previous findings. Canonical packet JSON is limited to 48,000 UTF-8 bytes and labels its ceil(bytes/4) estimate; this is at most an estimated 12,000 tokens, not a billed-token measurement. Larger work must be narrowed or split. CLI input files are bounded to 128 KiB and registry storage to 32 MiB including serialization overhead. Descriptor reads require regular files, reject redirection, enforce byte limits even after a stale size snapshot, and limit JSON nesting to 32. The complete retained check-name set, including ci-ok, is capped coherently at 32 before a round is issued.

A structured verdict identifies packet SHA-256, role, actual reviewer session, head, tree and source digest; `basis` explains what was examined. Findings have a stable per-review ID, priority, file/line and concrete reproduction/expected/observed behavior. P0/P1/P2 findings block; P3 is advisory. Empty findings are valid. A `clear` conclusion cannot conceal a blocking finding. Each follow-up verdict explicitly resolves every prior blocking finding by its task-history review key and concrete fix evidence. Store content hashes catch changed packets/reports; accepted reports cannot be overwritten.

Fixes or changed review source require another bounded round. A verdict bound to an old commit/tree/source cannot be recorded or used to gate the new one. Issued rounds and earlier findings remain in the same history. A pass requires every assigned role, no blocker, current actual clean source, and all original plus protected hosted check names passing at the same exact head. The returned pass proof includes the exact check snapshots and their hash; retain that output with the actual run evidence. After a pass, compare the live PR head again and merge normally with the expected-head guard. The protocol itself neither merges nor grants new authorization. If packet/report output was lost, use `cargo xtask review --state-root "$REVIEW_STATE" status M01.05` before retrying: it recovers the latest issued packet, count and accepted-report hashes without charging a round, and explicitly is not a current validation verdict. CLI output is UTF-8, including on a host whose console encoding is ASCII.

## Qualification scope

The protocol tests use actual Git repositories and deliberately seeded arithmetic, authentication and self-comparison-oracle regressions. Supplied independent finding reports must block integration; wrong reviewer, stale check/source/tree, hidden dirty bytes, omitted prior resolution, changed report and round-reset attempts must fail. These are deterministic protocol controls, not a claim that an AI detected the seeds. Both first-round independent sessions separately identified the arithmetic, mismatched-credential and self-comparison defects from source; M01.08 still owns broad frozen agent-quality, escaped-defect and whole-workflow cost evaluation. Provider/hardware/runtime search qualification is separate.
