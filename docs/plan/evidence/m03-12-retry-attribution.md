# M03.12 retry attribution

`provisional`: implemented in `crates/s3` and tested against the scripted in-process provider of M03.07 (`crates/s3/tests/attribution.rs`). No real provider or SDK.

## What exists

- `S3Store::with_retry(RetryPolicy { max_attempts, backoff })`; the default is one attempt, so nothing changes until a caller opts in.
- **What is retried.** A read after a 429, a 500, any 502-599, or a failure to connect. A write **only when no connection was made** (nothing was sent; in ureq the `ConnectionFailed` error is produced in the connect step, which was checked in the 3.5.2 source). A status such as 503 does not prove a write did not commit (a gateway or a store with a quorum failure can answer after applying it), so a write that got any status, or an unknown outcome, is not repeated: the caller replays the same request id, and the adapter's conditional-write path resolves an executed write by that id. A throttled write therefore reaches the caller as `Throttled`. No test of a 503 that really committed was written (the scripted provider cannot build the digest the resolution path checks), so this safety rests on never repeating it.
- **Each attempt is attributed and flagged.** A retry is a repeat attempt of one logical call: three attempts of one write show as 1 logical call, 3 attempts, 2 retries, amplification (3, 1), outcomes throttled twice and ok once, and the scripted provider's log shows 3 PUTs. A refused connection retried three times attributes 0 request bytes each time.
- **Retries spend the caller's budget.** Each retry charges the context's attempt budget and the request bytes (body, key and query); when the budget is spent or the deadline has passed the last real failure is returned, never a budget error standing in for it. The pause is capped at the context's remaining time. A `max_attempts` of 0 behaves as one.
- A POST (multipart control) attempt through the probe helper is attributed as the `Post` verb, billed as a write, and agrees with the provider log. The adapter itself still has no multipart upload path.
- A hairpin path through the adapter (`with_path`) lands in the NAT-hairpin class, is unknown-priced without a rate and priced with one.

## Tests and mutations

Twelve new tests in `tests/attribution.rs` (22 in the file). Mutations each seen red: retrying a write after a status, retrying an unknown outcome, letting a spent budget raise a budget error, charging retries without the key bytes, an uncapped backoff, and not flagging retries.

## What this does not do

- **No real retrying SDK was run.** The retry layer is the adapter's own; the claim is about this layer's attribution, not any provider SDK's hidden attempts. **No multipart upload path exists** in the adapter; the POST test attributes a hand-built probe request. Both, with conflicts under retry, are M03.13.
- **The provider log is not independent of the adapter's rules** for what a LIST is (a GET with a `versions` query), and `reconcile` compares verbs only; an independent classifier and outcome and byte comparison are M03.13.
- **Tenant feed and bounded telemetry are M08.02's.** `RequestContext` carries no tenant identity and the instrument catalog is an `hsearch-observe` and `hsearch-types` contract; M08.02 names both.
- A wrong-unit rate cannot be constructed without naming its unit (`Rate::per` takes a `Unit`), so there is no runtime rejection to test; M03.07 pins the GB-versus-GiB arithmetic.
- The first version of this task retried writes after a 429 or 503; review showed that was not safe to claim, and it was narrowed as above rather than the claim being kept.
