# M01.17 opaque forensic retention

This package implements a separate failed-artifact integrity operation. Source qualification and live operation approval are separate gates. Its presence grants no permission to inspect, pack or retire actual FFF artifacts. Native qualification, final independent review and protected integration remain required.

The selected scope is fixed to `hsearch-agent-fixture-3686832deb7c` and `sealed-export-qual3686832deb7c`: 84 nodes, 69 files and 224852 payload bytes. The opaque index preserves every recorded path, kind, mode, uid, gid, nanosecond mtime, Windows attribute, link count, size and file hash. It retains original CLI exit1, 601 guest tests, missing original host proof and an unconfirmed cause. Neither a verified pack nor its range views are successful original-job Readers or current qualification evidence.

## Components and authority

`protocol.py` validates fixed scope, source/image bindings, exact late-bound human wording, limits and the original deadline. Its authorization parser checks shape; it authenticates nobody. The trusted coordinator must retain the actual fresh human message and root disposition. An approval for planning or source work cannot serve as mutation authority.

`lifecycle.py` uses the current checkout's real TaskStore with task ID M01.17 and exactly its two owned paths. It does not call parent author admission or override a task identity. The component records `forensic-active`, `forensic-interrupted` or `forensic-complete` in the genuine claim. Accepted default TaskStore run, resume and cleanup refuse those phases. Only the component's narrowly bound recovery can reconcile a partially retired prefix. The parent claim and source stay unchanged.

The protocol's fixed `CLAIM` labels the original failed evidence owner, not the executing task. Versioned component admission names M01.17; root disposition and the genuine TaskStore claim bind its actual executing claim and owner. Historical M01.08 provenance cannot admit a new operation.

Read-only preparation requires an exact-source M01.17 review gate and root/owner preparation grant. Source and registry inspection uses bounded read-only published snapshots, avoiding the generic registry context manager's rewrite side effect. A pinned closed-job record is independently checked against controller birth, exact container-ID absence and the existing resource registry. Unknown processes, containers or leases refuse. Creating a new descriptor does not establish that its claims are true.

`storage.py` provides real filesystem/archive capabilities and an append-only metadata stream outside the archive. Every record has a sequence and previous-record hash, exclusive creation, checked writes, file fsync and directory fsync. Partial files and uncertain publications remain charged and are never overwritten. An orphan or broken chain stops loading. Mapping and audit durability are reaffirmed before unlink, including after a lost fsync acknowledgement.

`controller.py` builds one exclusive uncompressed USTAR, verifies complete members and ranges, publishes its durable map and audit, then removes only a deterministic prefix of the exact original identities. Files precede directories. Each unlink syncs its parent. A missing acknowledgement is reconciled by observing the exact missing prefix and verifying the remaining bytes/metadata and pack. Non-prefix loss, aliases, source changes or corruption refuse. A failure before a complete map/audit remains preserved for inspection; it does not trigger restaging or redispatch.

`backend.py` supplies an actual local Docker command transport. It requires a pre-existing canonical operation lease; it creates no allowance or resource pool. The existing registry's normal `acquire` API can record a separately authorized operation lease with the fixed budget and source/manifest/owner bindings. No such lease is created by source tests or automatically by the entrypoint. One probe checks actual cgroup limits; one fixed read-only worker verifies every pack member. The host rechecks every original byte and metadata field after the worker. Containers have no network, a read-only root, no capabilities and an unprivileged UID. Only the code directory, staged pack file and three pinned control-record files are mounted read-only. Private host directory modes remain unchanged. Newly created pack/control files are guest-readable beneath a checked private host ancestor; original files are never chmodded. A created ID is recorded before start. Unknown create outcomes remain blocked; cleanup uses only acknowledged exited IDs without force.

`worker.py` imports only this package and the standard library. It does not extract files, import semantic evaluators, write metadata or run arbitrary jobs. `_bytes.py`, `_pack.py` and `_sync.py` contain the explicitly inventoried incoming primitive closure. See [PROVENANCE.json](PROVENANCE.json) for original bodies, local changes, authors and the retained external parser failure.

## State and bounds

The mutating phase graph is admission, exclusive pack intent/create, checked member writes, pack file/directory sync, one probe/worker, map publication, complete audit publication, retirement intent, exact-prefix unlink/sync, final audit/accounting and terminal publication. Every exception leaves an interrupted genuine claim. It cannot become a passing receipt. Lifecycle finalization failures retain the primary exception and mark finalization uncertainty.

Recovery never extends the original wall/monotonic deadline or restarts a worker. It requires the same source, manifest, authorization, root disposition, archive/metadata/pack identities and durable map/audit. Before each I/O effect, a durable high-water reservation charges 8MiB byte blocks and individual rescan slots. A crash conservatively loses unused reserved bytes. Repeated recovery cannot reset the 256MiB/32-rescan pool. Metadata exhaustion stops further I/O even if another budget has room.

| Pool | Bound and admission rule |
| --- | --- |
| Physical archive | 4096 nodes and 64MiB, including the one staged pack; no maintenance exception |
| Selected originals | Exactly 84 nodes/69 files/224852 bytes, complete parent-directory closure, no links/reparse points/case aliases |
| Qualification reserve | No concurrent qualification in the forensic lifecycle; final physical occupancy plus the unchanged 42-node future reserve must fit4096 |
| Pack/index | 2MiB pack, 1MiB canonical index; USTAR representability checked before creation |
| Private preparation artifacts | At most two slots and 1MiB total in a separate explicitly owned directory; partial files remain charged |
| Durable metadata | 96 records, 72 journal events, 131072 bytes per ordinary record, 1MiB manifest record, 14417920 bytes total, including orphans |
| Operation I/O | 256MiB total and 32 rescans across recovery; worker's complete read bound debited before dispatch; 65536-byte range/read chunks |
| Source inspection | At most eight snapshots per controller instance; accepted TaskStore bounds each at10000 paths/128MiB, plus16 driver files of at most1MiB each; aggregate ceiling1152MiB per instance |
| Deadline | Original admission deadline, at most1200000ms; individual transport commands at most30s and no later than remaining operation time |
| Worker resources | 2000 CPU millis, 4GiB memory, zero swap, 256 PIDs, 2GiB tmpfs scratch; actual probe must match |
| Diagnostics | Aggregate transport stdout/stderr8KiB, public result16KiB; fixed error codes avoid payload disclosure |

The read-only preparation estimator refuses if `20*pack_bytes + 12*224852 + 8*outside_bytes` exceeds the operation I/O ceiling. That is a bounded workload envelope, not a quota increase. Metadata and source inspection are separately bounded control pools. File buffers are at most8MiB, the pack at most2MiB, and index/JSON inputs at most1MiB. JSON depth is checked before decoding; record serialization stops at its byte bound.

These are trusted-coordinator interlocks and filesystem identity bookends, not an atomic snapshot against a malicious concurrent host writer. The coordinator must keep source, closed originals and owned metadata quiescent. Unknown observations refuse. No worker/kernel/native success is inferred from injected test ports. Windows directory-sync failures remain failures; positive Windows file-operation tests explicitly substitute that port and do not claim native Windows durability qualification.

## Entry points and verification

The coordinator library APIs are `Controller.prepare`, `Controller.run`, `Controller.recover` and `ForensicArchive`. `python -B -m tools.forensic_retention_v1 --help` describes the explicit config/evidence entrypoint. It loads only existing canonical state. Preparation publishes a private manifest and a bounded hash/size summary. Run/recover require explicit content-bound evidence files. Audit uses a fresh bounded read-only allowance and cannot retire or qualify content. No live invocation is authorized by these examples.

Run author-local controls on fresh fabricated fixtures:

```sh
python -B -m unittest tools.tests.test_forensic_retention_v1 -q
python -B tools/ci.py docs
python -B tools/api_contract.py check
```

The controls use real file writes, file fsync, USTAR/member/range verification, map/audit publication, unlink and interrupted-prefix recovery on disposable files. An incoming independent oracle freezes original bytes and every metadata field before execution. Real default TaskStore methods run against fresh temporary Git/stores. Docker command boundaries are mocked; no actual Docker/backend/resource admission or FFF action follows from their success. The ordinary inherited suites and native x86_64/ARM64 matrix remain required. External oracle self-controls and the older35-test prototype are preserved separately and are not counted as subject qualification.

The existing sandbox-x64 and sandbox-arm64 jobs execute the Python subject suite through sandbox smoke's check mode, `cargo xtask check`, `tools/check.py`, and unittest discovery. Bare native Rust/coverage jobs do not run it. Final qualification needs exact-head test-name/count/zero-skip receipts from both sandbox jobs. Ordinary CI execution of the synthetic source suite does not establish observed operation-specific Docker transport/kernel behavior; that separate evidence remains unrun until an authorized qualification path executes it.

M01.17 R1 and its original review base remain counted. Implementation review must continue that history after deterministic checks and root disposition; this package cannot issue a review or merge itself. M01.08, older229/PAIRS/AUTH, new15, M02.10, original failures and all parent qualification gates remain untouched.
