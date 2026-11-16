# M03.13 attribution against an independent wire record

`provisional`: measured on one RustFS 1.0.4 (the digest pinned in `dev/images.json`) over loopback, plain HTTP, a shared 4-CPU container. No AWS S3, no MinIO. Relative numbers only.

## What exists

- **A wire record that does not use the adapter's rules** (`crates/s3/tests/wire/mod.rs`). A TCP proxy in front of the store parses each HTTP/1.1 request itself. It decides what a request is from the wire: a GET on the bucket is a LIST, `POST ?uploads` creates a multipart upload, `PUT ?partNumber&uploadId` is a part, `If-None-Match` or `If-Match` marks a conditional write. It counts the bytes it read from the client and wrote back, the status the client was given, and whether the client received it. It injects 503, 500, a refused request and a lost response.
- **A real SDK under the adapter's attribution.** `aws-sdk-s3` 1.159.0 (dev-dependency only; plain HTTP, no TLS feature, so no C toolchain) runs with its own standard retry loop. An interceptor reports every attempt, hidden retries included, through the new `S3Store::record_attempt` and `record_logical`, flagged as a retry from the second attempt of one call. Production code still uses `ureq`: the SDK is how we check the adapter's policy against a retry implementation it does not control.
- **Reconciliation compares attempts per (verb, outcome) and the byte totals of each class**, not only verbs. It compares the status the *client observed*: a write the store committed whose answer was lost is a transport failure to the client and a committed write on the wire. `committed_unobserved` counts those.

## Results (12 tests in `tests/independent.rs`, all green, 6 repeated runs stable)

| Scenario | Wire | Ledger |
|---|---|---|
| SDK GET, 2 × 503 then success | GET 503, 503, 200 | 1 logical call, 3 attempts, 2 retries, 2 throttled |
| SDK GET, connection dropped once | GET none, 200 | 1 transport attempt, 1 retry |
| SDK multipart, 3 parts, first part throttled once, conditional complete | POST, PUT 503, PUT, PUT, PUT, POST | 5 calls, 4 PUT, 2 POST, 1 retry |
| SDK conditional PUT, standard retry, response lost | PUT 200 (lost), PUT 412 | amplification (2, 1): the SDK lost to its own earlier write, shown as 1 conflict and 1 retry |
| SDK conditional PUT, retries off, response lost | one PUT 200 (lost) | 1 attempt, 0 retries, 1 transport |
| Adapter HEAD, 503 then dropped then success | 3 HEADs | 3 attempts, 2 retries |
| Adapter write throttled, then response lost | 2 PUTs, one per call | 0 retries; never repeated inside a call |
| Adapter lost race (412) plus resolution reads, LIST throttled once | PUT 412, LIST 503, LIST, HEADs | 1 conflict, throttled LIST retried |
| Adapter multipart, part throttled, abort | POST, PUT 503, PUT, PUT, POST / POST, PUT, DELETE | agrees |
| Adapter, 5 throttles armed, policy 3 | 3 HEADs, 2 faults never reached | last real failure `Throttled`; the context's attempt budget (2) wins over the policy |

Attempt counts and **bytes agree exactly** in every row, request and response. Exactness holds for RustFS over loopback HTTP/1.1 only: the response size uses the canonical reason phrase (AWS S3 sends `Slow Down` for 503, HTTP/2 has none), and the proxy's injected answers use the canonical text, so those rows do not test it. The wire bytes are what the proxy wrote, including its `Connection: close`.

## Residuals found and fixed, not rounded away

The first reconciliation did not agree on bytes. The adapter's request size was a lower bound that missed `user-agent`, `accept` and `content-length` (37 to 59 B per request); its response size missed the reason phrase (2 to 19 B). Both were exact functions of what the client sends, so the adapter now sets `user-agent` and `accept` itself, counts `content-length` on PUT and POST, and adds the canonical reason phrase. The SDK's request bytes agreed with the wire from the first run. Two proxy artefacts were also found and fixed in the proxy: a synthetic HEAD answer carried a body, and a log entry was written after the response, so a fast client could read the log first.

Remaining assumption: the response size uses the *canonical* reason phrase. A store that sends a different phrase would differ by the length difference.

## Seen red

Each mutation of `crates/s3/src/lib.rs` made tests fail: `record_attempt` dropping the retry flag (4 failed), not counting `accept` (6), not counting the reason phrase (6), retrying writes after a status (2), recording POST as PUT (1). A GET attributed as a LIST, and an attempt the ledger lost entirely, both fail the reconciliation (`a_misclassifying_attribution_path_fails_the_reconciliation`).

## Benchmark and trade-offs

`cargo run --release -p hsearch-s3 --example bench`, 2 s per cell, before and after, same container: 1 KiB PUT 173 then 183 per s, 16 MiB PUT 107.4 then 104.8 MiB/s, 16-writer 64 KiB PUT 649 then 751 per s, sparse CAS commit p50 9.8 then 11.5 ms. Everything is inside run-to-run noise: the change adds a few small per-request strings (the two header names and values and a content-length string, under ten bytes each); a single run per cell, no variance measured, so a 16% swing in either direction is not distinguishable from noise here. Per commit: 1.00 PUT and no other request, unchanged.

Downsides: a heavier dev dependency tree (about 190 crates, tests only; `docs/plan/dependencies.json` records it, no new C dependency); the SDK is a check, not the production client; `record_attempt` is a public API that trusts its caller's status and sizes.

## Review follow-ups

An independent review found: the outcome table in the proxy mirrors the adapter's (only verbs are independent), so a misclassified outcome is now also tested by hand (a 412 recorded for a 200); the retry flag cannot be reconciled with the wire and is pinned by exact equalities (the lost-race test now asserts exactly one retry); backoff is now tested (two growing pauses, at least 120 ms for 40 ms steps); the pre-send path is tested with a refused connection (3 attempts, 2 retries, 0 request bytes). `record_attempt` trusts its caller, and the test interceptor keeps one attempt counter per client, so SDK calls on one client must be sequential.

## Not done

- The SDK is not wired as the production client. The adapter keeps its own retry policy (reads only; a write only when nothing was sent), which the SDK test shows is the safer rule for conditional writes: the SDK's default retries a lost conditional PUT into a conflict.
- Backoff is the adapter's linear pause capped at the remaining deadline, without jitter.
- MinIO was not run (no pinned image is available); the proxy tests need only an S3 endpoint.
- Tenant feed and telemetry stay with M08.02.
