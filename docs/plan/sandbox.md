# Local Linux sandbox

M01.02 supplies a non-root development image and an isolated RustFS/test-CA topology. It does not implement the operational task/worktree/review runner in M01.03-M01.08, the storage capability suite in M03, or production authentication in M05. The [ledger](tasks.json) retains those obligations.

## Start from a fresh checkout

Run the controller on the host with Python 3.11+, Docker Compose and a local Linux Docker engine with cgroup v2 and working memory/swap/CPU/PID controllers. Driver `none` and missing controllers are refused; cgroup v2 alone is insufficient, as [Docker's rootless resource reference](https://docs.docker.com/engine/security/rootless/tips/#limiting-resources) explains. The default build plus one storage service requires admission for 3 CPUs and 6 GiB within the host budget. Free host/backend disk must cover **all outstanding reservations plus 10 GiB headroom**, not just the newest request. This means 20 GiB for a new topology, 28 GiB for its default build with no other claims, and about 30 GiB for the two-topology probe. The controller reserves half the engine memory (at most 16 GiB), leaves a CPU for the host (at most 8 leased CPUs), and refuses excess reservations. These conservative limits do not control unrelated applications.

Use the **same host registry path for all hsearch worktrees/controllers**. The example uses a path you choose in a private writable host directory. Windows runs the controller with the user's normal host token; build/test processes run in Linux containers. The registry and Docker socket are never mounted into a development container.

```sh
python3 tools/sandbox.py --state-root /private/hsearch-leases image
python3 tools/sandbox.py --state-root /private/hsearch-leases up M01.02
```

`up` prints a generated `hsearch-...` namespace after verified TLS readiness. Substitute that exact value below:

```sh
python3 tools/sandbox.py --state-root /private/hsearch-leases run NAMESPACE prefetch
python3 tools/sandbox.py --state-root /private/hsearch-leases run NAMESPACE diagnostics
python3 tools/sandbox.py --state-root /private/hsearch-leases run NAMESPACE check
python3 tools/sandbox.py --state-root /private/hsearch-leases run NAMESPACE probe
python3 tools/sandbox.py --state-root /private/hsearch-leases down NAMESPACE
```

`cargo xtask sandbox ...` forwards the same arguments when a host Rust toolchain is available. The direct Python entrypoint bootstraps a machine without a host Rust installation. `status` reports recorded leases and phases. `smoke` exercises fresh dependency prefetch, diagnostics, the complete bootstrap suite, two private storage topologies, actual TLS/S3 probes, and cleanup of one topology while the other remains usable.

`down` permanently removes that namespace's declared Docker volumes, including its test data, keys and dependency/target caches. It first verifies source ownership, lifecycle ownership, active command leases, container identities and resource labels. It never runs a global prune or recursively removes host source. Small immutable Compose snapshots remain in the ignored `.hsearch-local` directory for diagnosis.

## What is bounded

| Process | CPU | Memory | PIDs | Network |
| --- | --- | --- | --- | --- |
| Build/test/diagnostics | 2 | 4 GiB | 256 | None after prefetch |
| RustFS | 1 | 2 GiB | 256 | Task-private internal bridge |
| Test CA initialization | 0.5 | 256 MiB | 64 | None |
| TLS/S3 probe | 0.5 | 256 MiB | 64 | Its task's internal bridge |

All runtime processes use UID/GID 10001, a read-only image root, dropped capabilities, `no-new-privileges`, disabled swap and bounded tmpfs. Effective memory/swap/CPU/PID caps are verified before CA generation, storage probes or repository commands; unlimited or oversized cgroup values refuse execution. Source is mounted read-only. Mutable Cargo/target volumes belong to the namespace; concurrent commands cannot lease the same mutable pair. Explicit public-network prefetch receives no storage credentials. Storage probes receive only their own development credentials and public CA material. The private CA key is mounted only into initialization.

Host reservations serialize through a kernel file lock and atomically replaced registry. Resource claims survive controller exit. Age alone never frees a lease. Recorded process identities distinguish PID reuse; starting/stopping lifecycle owners and live/uncertain command leases prevent cleanup races.

Disk is an admission reservation plus sampled cache/target accounting, **not a filesystem quota**. A topology retains a 10 GiB reservation, including its bounded 8 GiB Cargo/target allowance and test storage allowance. Host and Docker-backend probes preserve 10 GiB headroom against all outstanding reservations. Each workload rechecks backend space just before launch, sampling the actual cargo/target/data volume filesystems read-only as well as the image filesystem; a ready topology is not a lasting free-space certificate. The command monitor counts allocated/file bytes without intentionally following symlinks, stops at a bounded entry count, and terminates only its own child process group on excess or deadline. Sampled accounting can lag writes; the Docker memory/CPU/PID/tmpfs limits are hard runtime controls. The build recipe itself is trusted host setup under a resource reservation; it adds exact-version Rust components to an immutable base and does not execute repository tests in an uncapped image build.

## TLS and service isolation

Each topology has distinct named volumes, a private network, a private serving address, bucket names and randomly generated development credentials. Storage port 9000 is available only in its network namespace; no storage port is published to the host. Both the controller and the smoke check assert that boundary. [Docker's internal-network contract](https://docs.docker.com/reference/cli/docker/network/create/#network-internal-mode---internal) describes isolation between these bridges.

The initializer generates a two-day CA and server certificate with `rustfs`/`localhost`/loopback SANs. It refuses partial initialization or changed/expiring credentials. The pinned RustFS release reads `rustfs_cert.pem`/`rustfs_key.pem` and credential files from separate read-only mounts; [RustFS's TLS reference](https://docs.rustfs.com/en/integration/tls-configured) documents the filenames. Readiness uses verified HTTPS, so plaintext fallback cannot pass. The live probe rejects an untrusted CA and an invalid S3 signature, then performs a bounded signed bucket/object round trip. Its fixed-path SigV4 helper is a test driver, not a general S3 client or a provider-conformance substitute.

The Rust base, components and RustFS image are pinned in the build recipe and `dev/images.json`. Runtime uses the locally built immutable image ID, indexed by the recipe hash and engine identity. Separate recipes cannot replace each other's image record. No hsearch source, credentials or host Docker socket is baked into the image. Upstream images retain their licenses; this change publishes build recipes only and pushes no container image or release.

## Refusals and recovery boundary

Low disk, engine unavailability, nonlocal/unsupported engines, exhausted CPU/memory/PID/disk admission, stale images, changed Compose snapshots, foreign resource labels, changed container identities and active/uncertain owners refuse the operation. Diagnose with `status` and the exact recorded namespace. Do not clear a lease just because a timestamp is old or a command timed out. A failed read-only preflight that is known to have completed releases its empty claim; an uncertain setup/build/run retains its claim for inspection. A momentarily empty container listing never resolves an uncertain create; run claims are released here only after successful synchronous completion and absence of the completed container.

Automatic crash recovery and live-handle reclamation are M01.03/M01.08 deliverables. This controller intentionally refuses ambiguous cleanup rather than guessing that an owner has died. The tests cover low-resource refusals, concurrent admission, lifecycle exclusion, unrelated-process survival under deadline/disk termination, and foreign-resource cleanup refusal. The live two-topology smoke verifies independent stores remain usable after targeted cleanup. See [task evidence](evidence/m01-02-sandbox.md) for observed results and remaining qualification.

The M02.09 research continuation tightens the shared build/initialization/monitor PID cap from512to256, preserving CPU/RAM/swap/disk/deadline and every original refusal/cleanup control. Complete ordinary suites remain, with three additional fresh-process native experiment repetitions in each dev/release profile. Current semantic research acceptance still requires final-source review and both native targets; the historical512-PID receipts are retained as history, not the stricter envelope qualification.
