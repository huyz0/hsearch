# M03.06 evidence: provider qualification, example provider, format compatibility

`provisional`: RustFS 1.0.4 (digest pinned in `scripts/rustfs.sh`) in Docker on a shared cloud VM, loopback, one run. Counts and pass/fail are not timings; no timing is claimed here.

## What was built

- **The shared suite** (`crates/testkit/src/provider.rs`, suite id `hsearch-object-provider/2`). `qualify(name, backend, capabilities)` runs eight checks against any `MetadataStore`: `conditional_history`, `stale_update_refused`, `ranges_exact_and_key_bound`, `changed_identity_refused`, `budget_refused_before_effect`, `cancelled_context_creates_nothing`, `one_winner_among_concurrent_updates` (8 threads) and `remove_version_declared` (a provider that does not declare deletion must answer `Unsupported`; one that declares it must delete). `qualify_upload` adds the create-only multipart probe for a `LargeObjectWriter`. The checks reuse `crates/object/tests/support`, the histories the object crate already runs, by `#[path]` include, so there is one predicate and no second copy. A report is a JSON artifact (`Report::to_json`).
- **Fail closed.** A check that panics is caught and reported with its message; one that times out marks the provider stalled and the remaining checks are `skipped`, so a stalled provider is reported in about 2 s instead of waiting out every check. An error no contract names (`Corrupt`) is a failure, never a pass.
- **External provider example** (`crates/example-provider`, `hsearch-example-provider`, not in `default-members`): a single-process directory store built on the public `hsearch-object` contracts only. Per-key `CURRENT` pointer flipped by rename as the commit point, identity records for resolution, keys limited to 100 bytes. A crash between the flip and the identity record answers `ResolutionUnavailable`, never a guess. It passes the full suite and its state and identities survive reopening.
- **Format compatibility matrix** (`docs/plan/format-compatibility.md`, `crates/format/tests/compat.rs`): all 12 kinds are held to the same promises (current version reads, newer and zero versions refused, unknown mandatory capability refused, oversize body refused, no kind reads another's object), and the doc table must equal the `Kind` enum (code, name, version, max body) or the test fails.

## Results

| Provider | Passed | Artifact |
|---|---|---|
| memory (reference fake) | 8/8 | `evidence/conformance/memory.json` |
| example directory provider | 8/8 | `evidence/conformance/example-directory.json` |
| RustFS 1.0.4 through `S3Store` | 8/8 | `evidence/conformance/rustfs.json` |

Negative providers (`crates/testkit/tests/provider.rs`): disabled (every call `Unavailable`), no conditional writes (`Unsupported`), no fencing (stale version accepted), unknown failure (`Corrupt`), stalled and panicking providers are each refused; the no-fence provider fails `conditional_history`, `stale_update_refused` and `one_winner_among_concurrent_updates`. A store declaring a capability it lacks (or denying one it has) fails `remove_version_declared`.

## What the suite found

Run against RustFS the suite failed `ranges_exact_and_key_bound`: a range with offset `u64::MAX - 1` was answered `NotFound`, because the store rejects an offset it cannot parse with 400 and the adapter reads 400 as a missing version. The contract says `InvalidRange`. Fixed in `S3Store::read_range`: no S3 object reaches 2^62 bytes (limit 5 TiB), so such an offset is answered `InvalidRange` before any request. Downside: a ghost version at that offset reports `InvalidRange` rather than `NotFound`.

## Mutations seen red

- Removing the range guard fails the RustFS suite test.
- Raising the pin body bound by one byte fails the compatibility doc check.
- The negative providers above are the mutations of the suite itself.

## Limits

- **Module drain failures** (the ledger's validation list): no provider registry, module lifecycle or drain exists in the code yet (M04 and later own it), so there is nothing to drain. What is covered is the provider side: a disabled, missing capability, unknown-error, stalled or panicking provider is refused. Drain failure tests belong with the module lifecycle.
- The example provider is single process: its mutex orders writes, so it does not claim cross-process fencing, and a shared filesystem would need the real CAS primitive of that filesystem. The suite's concurrency check uses threads, not processes or hosts.
- The suite does not inject faults into arbitrary providers (lost replies, partial writes); those histories run on the reference store (M03.08, M03.05 models).
- `S3Store` does not implement `LargeObjectWriter` in this suite's sense; the multipart probe for it is the existing `a_create_only_upload_passes_the_shared_probe`.
- Coverage floors were not measured locally (`cargo llvm-cov` is not installed in this environment); CI's coverage job is the measure.
- One RustFS version, one host, no MinIO (M03.10 tracks it) and no real S3.

## Reproduce

```
eval "$(scripts/rustfs.sh up)"
HSEARCH_S3_REQUIRED=1 HSEARCH_CONFORMANCE_DIR=$PWD/docs/plan/evidence/conformance cargo test -p hsearch-s3 --test rustfs
HSEARCH_CONFORMANCE_DIR=$PWD/docs/plan/evidence/conformance cargo test -p hsearch-testkit --test provider
HSEARCH_CONFORMANCE_DIR=$PWD/docs/plan/evidence/conformance cargo test -p hsearch-example-provider
cargo test -p hsearch-format --test compat
```
