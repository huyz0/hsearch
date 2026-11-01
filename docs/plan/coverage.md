# Source-bound coverage integration

Status: M01.12 private integration prototype, not accepted. M01.11's [source-proof component](source-proof.md) is [accepted](evidence/m01-11-integrated.md). Original M01.07 remains blocked at its hard third round. Reused collector machinery is subject to full independent review of this redesigned integration and every [retained original finding](evidence/m01-07-review-history.json); its old gate is never cleared or reset.

The collector runs pinned Rust production builds with `-C instrument-coverage -C link-dead-code`. Empty-profile maps from actual production objects define the denominator, including ordinary/unused generic code. Test-binary runtime maps contribute hits only to that denominator. Runtime-only tests/development sources are distinct from code compiled into production; an undeclared root/shared/development/external production map refuses instead of disappearing. Actual Cargo metadata, dependency/toolchain roots and bound build-script outputs constrain ownership. Declared source entries and owner metadata are rederived at merge.

Context version2 binds the complete accepted immutable source proof, actual head/base/source bytes, manifest owners, target/profile/features/flags, tool hashes/effective compiler, test inventory, production objects, exact binary shards and raw-profile records. Merge independently recomputes source proof at both ends, rehashes all physical inputs and refuses missing/stale/duplicate/extra artifacts. Changed lines are derived from verified proof only; a separate supplied list must match. Human patch text has no executable path. Source bytes and every line-oriented compiler/report protocol use LF framing; Unicode separators remain content.

All production aggregate/crate/file/changed-line floors remain>=90% in both dev and release. No executable lines means N/A with an exact-source reviewed audit, never100%. Missing instrumentation is an error. Ordinary tests keep the pinned nextest retries0/threads2/fail-on-timeout policy; explicit doctests, Clippy warnings/unsafe refusal and all applicable features remain separate required checks. Effective compiler/test environment overrides cannot silently weaken the policy. The runner pins the probed compiler, sets both compiler-wrapper environment variables to empty (disabling Cargo-configured wrappers), and refuses even an empty encoded-flags environment variable because it takes precedence over instrumentation flags. The actual Cargo control first observes each configured nonexistent wrapper fail, then succeeds with the explicit disabled policy; toolchain context records those effective wrapper values. These rules follow the [Cargo environment reference](https://doc.rust-lang.org/cargo/reference/environment-variables.html) and [flag precedence](https://doc.rust-lang.org/cargo/reference/config.html#buildrustflags). Both native targets remain required before acceptance.

## Base and event policy

A valid exact ancestor is required. An explicit `parentless-root` marker qualifies only an actually proven parentless root. CI's root event markers may normalize to that marker only after the immutable proof confirms the root; missing/nonancestor evidence on any ordinary history refuses. Impact uncertainty still chooses the full suite, but it cannot substitute all production lines as an unknown-base changed-percentage denominator. PRs use their real base, main pushes use the event's previous main, and manual dispatch requires `coverage_base`. Actual PR/main/manual event qualification is required; unit labels do not establish hosted event behavior.

```sh
python tools/coverage.py run --base <exact-ancestor-sha> --output /private/new-context
python tools/coverage_local.py --state-root /private/owned-leases --base <exact-ancestor-sha>
```

The local entrypoint freezes a clean source bundle, verifies host/Linux byte identity, runs both profiles in the existing owned non-root BUILD sandbox and retains bounded public evidence. It preserves TLS/storage isolation, BUILD/STORAGE caps and uncertain-outcome/owned cleanup rules. Direct native CI also retains finite command argument/output/disk/deadline/thread guards and its job deadline; these flags alone are not a claim of a separate hard host memory sandbox. Artifacts preserve measurements/manifests, not dependency binaries or shared lease/TLS-key directories.

## Explicit retired-test migrations

| Retired approximation/protocol | Required replacement oracle |
| --- | --- |
| A mode-only display change meant whole-file delta | Identical immutable blobs have zero added head lines; the complete proof records modes and zero delta. |
| Unknown base meant all-source changed coverage | Refuse unknown/nonancestor input. Only a proven root preserves the original all-physical-lines `[1,2,3]` oracle. |
| Mock hunk headers/body drove line identity | Actual before/after blobs preserve original additions `[2,3,4]` while deleted/context lines do not count. Missing/truncated/corrupt proof cannot become empty success. |
| Unicode source created fake patch records | Exact retained fixture attributes A`[3,23,24,25]` and B`[]`; aggregate10/11 passes but changed0/1 fails, never changed N/A. |

Original b637769 source/tests/reports stay frozen. These migrations were required by the [accepted redesign](coverage-redesign.md), not oracle changes to excuse a failure. The first private component run retains four expected old-parser-contract errors; corrected real Git/proof cases and all unchanged controls subsequently passed. A later malformed NUL test literal was corrected without altering its refusal assertion. The first full collector run safely refused its new merge inventory's literal backslash-zero delimiter; shared NUL inventory parsing and a real Unicode-path regression corrected it without removing the guard. At private head6e4b6f6, full Linux qualification passed274 component tests and5 real compiler controls, and actual dev/release collection each reported43/43 production lines (changed N/A because production Rust bytes were unchanged). Later compiler-config hardening requires fresh exact-head qualification. Independent final-tree review, full native PR CI, durable gate, normal protected merge and complete main/manual evidence remain pending for this prototype.

No generic coverage, runtime API, provider/fleet, original ARM causal diagnosis, release or deployment acceptance is inferred from component checks. M01.09 remains unconfirmed and M01.08 remains blocked.
