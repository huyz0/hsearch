# M01.02 sandbox evidence

Status: review. Base is integrated M01.01 commit `81bde7398bec3ebb50a82f9661f50faae1a02852`. M01.02 does not close the rest of M01 or implement a search service.

## Concrete artifact

The [sandbox instructions](../sandbox.md) specify entrypoints, image pins, resource limits, private serving networks, CA/credential mounts, refusal behavior and recovery limits. `dev/images.json` records immutable multi-architecture Rust 1.97.1 and RustFS 1.0.4 images. The dev image defaults to UID/GID 10001 and supplies Python 3.11.2, OpenSSL 3.0.20 and exact-toolchain rustfmt/Clippy/LLVM components. Runtime image records bind the recipe hash and engine identity; the Dockerfile sent to the builder is the exact hashed source. No image is pushed.

Host admission uses a shared kernel-locked registry. Starting/stopping owners and active/uncertain run claims exclude conflicting cleanup. Build/test/diagnostic containers have non-root read-only roots and actual 2 CPU / 4 GiB / 512 PID / 64 MiB tmpfs limits. Mutable cache/target volumes are per namespace. Disk admission preserves host/backend headroom and sampled command accounting without claiming a filesystem quota. The image recipe is trusted host setup under a reservation; repository checks run inside capped containers.

Port 9000 is served only in each topology's internal network, with no host ports published. Distinct networks/addresses/buckets are checked. Private CA material, server certificates, public client trust material and unique credentials have separate owned volumes. No static credentials are committed. Verified HTTPS readiness rejects plaintext fallback. The fixed-path SigV4 driver is harness smoke, not M03 conformance or production mTLS/BYOK.

## Pre-review local qualification

The [successful final image/smoke log](m01-02-local-check.txt) records:

- Recipe-bound non-root image built from its immutable base and pinned RustFS image fetched.
- Fresh isolated dependency prefetch passed, with about 606 MB peak accounted disk. Subsequent networking-disabled checks passed formatting, all-target/all-feature Clippy, debug/release Rust tests, release-library build, planning/model checks and **23 Python tests with no skips** on Linux.
- Diagnostics reported native x86_64, UID 10001, exact Rust/Cargo 1.97.1 and actual cgroups: `memory.max=4294967296`, `cpu.max=200000 100000`, `pids.max=512`.
- Complete bounded checks took about 36 seconds, with peak cache/target accounting `883950233` bytes. This is a local bootstrap observation, not a service SLO.
- Both real RustFS stores rejected wrong CA and credential inputs, then round-tripped distinct bounded objects through HTTPS/SigV4.
- After ownership-checked cleanup of the first topology, the second still passed its probe. Distinct internal networks/serving addresses and no published host ports were asserted.
- Final inspection found an empty lease registry and no remaining owned containers. Immutable image layers and small ignored Compose snapshots remain. No global prune or recursive source deletion ran.

Refusal controls cover CPU/memory/PID/disk overcommit, host/backend low disk, unavailable engine, bad task/path shapes, foreign ownership, live lifecycle owners, uncertain run outcomes and concurrent admission. A real two-process lock trial admits only one request when one fits. Linux disk/deadline controls terminate their own child group while an unrelated child remains alive. Disk scanning tests entry bounds and a static symlink loop. Capacity sensors in pure registry tests are explicit fixtures, so a small test tmpfs does not stand in for the host disk; real admission uses real probes.

The restricted Windows runner initially denied private redirected temporary-directory writes. Fresh permitted task-local directories under the normal host token passed; denied temporary paths were not retried. Earlier live attempts rejected an invalid host-NAT assumption on an internal bridge, then exposed the unit-fixture disk-sensor assumption. Both were fixed while retaining refusal tests and strengthening the private-port assertion. The plan's negative unfinished-dependency fixture now explicitly makes its candidate unfinished after M01.01 shipped; real dependency validation is unchanged.

## Independent review corrections

Separate correctness and security sessions reviewed `4982a064d266b379f0e71a4e542f37c1ee509f8f` and found three blockers: P2 uncertain run/create claims were freed after an empty listing; P2 host/backend free-disk checks omitted outstanding reservations; P1 cgroup-v2 admission and printed diagnostics did not prove effective hard limits on unsupported/rootless engines.

The corrected version retains uncertain claims regardless of momentary visibility, accounts for every outstanding disk reservation plus headroom on both filesystems, rejects missing resource controllers/driver-none, and verifies finite non-root effective CPU/memory/swap/PID limits before repository commands, CA generation and probes. Swap is disabled explicitly. Empty-listing, aggregate unconsumed reservation, missing-controller, unlimited/oversized-cgroup and public diagnostic/pre-execution counterexamples were added.

[Observed correction validation](m01-02-review-fix-check.txt) includes a fresh two-topology smoke with the full offline suite and 27 tests, actual `memory.swap.max=0` alongside the prior finite limits, all live TLS/auth/object/isolation probes and cleanup success. Additional targeted Linux regression verification passed all 29 tests without skips, including public diagnostic refusal and pre-execution checks. Pure fixture tests supply sensors intentionally; the complete live smoke reads actual kernel caps and disk probes. Follow-up independent review of the corrected commit and hosted native results remain required.

## Remaining acceptance

Independent final-tree correctness/security review and native hosted x86_64/ARM64 CI must pass before integration. Windows is a host-controller path with Linux containers, not native hsearch runtime qualification. Automatic crash reclamation, worktree/task lifecycle, full review/CI/coverage gates and coordinator recovery remain M01.03-M01.08. Provider/fencing/security/scale/soak qualification stays in its original later milestones. No release or deployment occurred.
