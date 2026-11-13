# M03.02 RustFS probe and M20.01 adapter evidence

Measured on a Linux cloud container (Docker 29.9, 4 vCPU) against the digest-pinned image in
`dev/images.json`: `rustfs/rustfs:1.0.4@sha256:ed9210d6411d77ffceb70d94b964a3cb7d225983368bc25bebe01e995293dcc2`,
one container, plain HTTP on loopback, `RUSTFS_CONSOLE_ENABLE=false`, bucket versioning enabled by the
probe. Started by `scripts/rustfs.sh up`. Numbers and behaviour here are **provisional**: single node,
no TLS, no fault injection at the network layer.

## What the probe proved (`cargo run -p hsearch-s3 --example probe -- run`)

| Capability | Result |
|---|---|
| Bucket versioning can be enabled and read back | pass |
| `PutObject` with `If-None-Match: *` creates a new key | pass |
| The same, on an existing key | refused, HTTP 412 |
| `PutObject` with `If-Match: <etag>` on the current version | pass |
| The same, on a stale ETag | refused, HTTP 412 |
| `If-Match` on a missing key | HTTP 404 (mapped to Conflict) |
| Every PUT returns `x-amz-version-id`, including identical bytes | pass |
| Ranged `GET ?versionId=` of a replaced version returns that version's bytes | pass |
| A range running past the end is truncated (206) | adapter checks length and fails |
| A range starting past the end | HTTP 416 |
| Unknown version id | not found |
| `ListObjectVersions` returns newest first, with `prefix` and `max-keys` | pass |
| 20 rounds of 16 concurrent compare-and-set clients | exactly one winner per round, the store's head is the winner |
| Acknowledged object and its request identity after `docker restart` | present and resolvable |

The probe exits non-zero on any required failure, and CI runs it plus the restart check on every push.

## Known property: ETag-based fences admit ABA on identical bytes

`If-Match` compares the ETag (a body hash). Re-putting identical bytes over a version keeps the ETag, so
a conditional update against the older version can succeed. Version ids differ, so reads stay exact.
Callers that need a strict fence include a sequence number in the payload (the M20.03 root does). A retry of a `Matches` update with identical bytes also passes `If-Match` and writes a new version rather than replaying, for the same reason.

## Not covered by this probe (moved to M03.10)

Multipart-upload failure, and response loss injected at the network layer against the real store. The
adapter's unknown-outcome and resolution paths are exercised against the real store by replaying
accepted identities, and the dropped-connection mapping against a local socket, but not by losing a
real RustFS response. MinIO as a second target could not be pulled in this environment
(`docker.io/minio/minio` is gone and quay.io returned 401), so M20.01's second-target run is open.

## M20.01 adapter results

`crates/s3` (`hsearch-s3`): `ObjectBackend` and `MetadataStore` over the S3 API. The M03.01 public
history (`conditional_history`), the 8-way simultaneous CAS test, lost-race and replay, identity
conflict, bounded resolution, typed error mapping and budget/cancel/deadline tests pass against the
pinned RustFS (13 integration tests, 9 unit tests). Line coverage of the crate: 95.7%, measured with
`cargo llvm-cov` while RustFS was running (floor 90%).

Request cost, counted by the adapter per operation type: a successful write is one PUT; a head is one
HEAD; a ranged read is one GET; a lost race or an unknown outcome costs one LIST plus one HEAD per
version newer than the precondition (at most 16). No LIST on any read, write or startup path.

## M03.10: response loss and multipart on the real store

Provisional: RustFS 1.0.4 (same digest), one container, loopback, debug build. Run with `cargo test -p hsearch-s3 --test fault` (needs the `HSEARCH_S3_*` environment, as `rustfs.rs`). A small std-only TCP proxy sits between the adapter and RustFS and can forward the next PUT and then close the client connection without relaying the answer (lost response after commit), or close it before anything reaches the store (lost request). The adapter's own signed requests are used throughout; multipart is probed through `S3Store::probe_request`, a `#[doc(hidden)]` escape hatch that is not part of the storage contract.

| Case | Observed |
|---|---|
| Create, response lost after RustFS committed | the adapter reports `UnknownOutcome`; `resolve` finds the write by identity and returns the original receipt; re-sending the same request returns the same version, no second version |
| Create, request lost before RustFS saw it | `UnknownOutcome` to the client; `resolve` answers `ResolutionUnavailable` (certified absent); the retry is then the first write |
| Compare-and-set, response lost after commit | `UnknownOutcome`; `resolve` and a replay return the same receipt; a rival holding the old token gets `Conflict` |
| Multipart: parts uploaded, never completed | `HEAD` is not found; no partial object |
| Multipart: complete naming a part never uploaded | HTTP 400 `InvalidPart`; key still absent |
| Multipart: abort, then upload a part | abort succeeds; the late part is HTTP 404; key absent |
| Multipart: complete with `If-None-Match: *` onto a new key | HTTP 200; one version of the right length |
| Multipart: a second complete with `If-None-Match: *` onto an existing key | HTTP 412; the first version is still current |

The three response-loss tests were run with the proxy's fault removed and failed (three of five red), then restored and green; the multipart cases record store behaviour, not adapter behaviour.

**Not done: MinIO as a second target.** `docker.io/minio/minio` is gone and the quay.io image needs credentials; pulling an unvetted image from another registry is not done under the project's rule against running unvetted binaries. The contract suite has therefore not been run on MinIO; M03.10 stays open for that, and the adapter's multipart upload (files above 16 MiB) is still unimplemented: this probe shows RustFS's multipart is safe to build on, not that the adapter uses it.
