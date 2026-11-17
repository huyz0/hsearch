# M05.08: KMS path accounting (emulated provider; the real-provider run moved to M15.08)

`provisional`, **emulator only**. The task asks for request counts, latency histograms and throttle, denial and timeout behaviour on a real KMS. There is no AWS, GCP or Azure account for this project and the M15 adapters do not exist, so **no real-provider number was measured and none is claimed**. What this delivers is the part that does not depend on the provider: the instrument, and the call counts, which are a property of the `KeyProvider` contract and the key manager. **Deferral (coordinator's decision, 17 November, following the M03 precedent for real-S3 checks):** M05.08 closes on this emulated accounting, and the real-provider latency, throttle and quota measurement is the new task M15.08, which depends on the real adapters and accounts (M15.01 to M15.03). The real run is one substitution away: replace `provider()` in `examples/kms_path.rs` with an adapter.

Code: `crates/security/src/metered.rs` (`MeteredProvider`: counts every attempt by operation and outcome, power-of-two latency histogram per operation, no key material), `crates/security/tests/kms_paths.rs` (11 tests), `crates/security/examples/kms_path.rs` (the bounded runner: fixed event counts per path, aborts past 20,000 calls or 600 s).

## Provider calls per request path (measured through the manager; counts apply to any provider)

| Path | Calls | Notes |
|---|---|---|
| New object | 1 wrap | The writer's own first read is a cache hit: 10 objects written and read back is 10 wraps, 0 unwraps |
| Warm read | 0 | 10,000 reads, 0 calls |
| Cold read, cache lifetime expired | 1 unwrap per object per lifetime | Three lifetimes of 1,000 reads each is 3 unwraps |
| 64 concurrent cold readers of one object | 1 unwrap | One flight, the other 63 wait on it |
| Cache effectively off (lifetime 1 ms, every read cold) | 1 unwrap per read | 500 reads, 500 unwraps: the amplification the cache removes |
| KEK rotation noted at the manager | 0 | New wraps use the new version; an old object opens with 1 unwrap once its cached key expires |
| Rewrap of K objects to a new key version | K unwraps (cold) + K wraps | The contract has no native rewrap, so this is 2K calls, once per object per rotation |
| 50 objects, 10,000 reads | 50 wraps, 0 unwraps | Calls scale with objects, not reads |
| Revoked key, caller reads again and again | 1 unwrap per attempt | 100 refused reads are 100 calls |
| Throttled x2, timeout, unavailable, then success | 5 unwraps | Every attempt is counted, including failures |

Price at the M02.07 card (USD 0.003 per 1,000 requests): 1 call costs USD 3 per million events; every row above is that number times its calls per event. A warm read costs nothing, a new object USD 0.000003. At the object-profile rate of one wrap per batch the M02.07 envelope is unchanged by this task (its one-wrap-per-batch assumption matches the measured one-wrap-per-new-object).

## Emulated latency (not a measurement)

`kms_path` injects 5 ms into every provider call. The histogram upper bounds it prints (about 5.1 to 6.4 ms at p50 and p99 for 200 to 1,000 calls) only show the instrument works: they are the injected latency plus scheduler noise on a shared 4-vCPU VM. A stalled provider (30 s against a 100 ms budget) shows as one `timeout` outcome with a maximum near the budget, never a hang (tested).

## Findings that need a decision

1. **No negative caching: a failing provider is called once per attempt.** A revoked key, a provider outage and throttling all reach the provider on every read of a cold object (100 refused reads is 100 calls; an outage across a cold fleet is a retry storm at read rate). The policy gate (M05.05) stops the revoked case before the manager when callers consult it; outage and throttle have no such stop. A real provider's rate limit would turn this into more throttling. Handled in M05.07 as a setting (bounded failure backoff per key, off by default only if measurement says it hurts), not here.
2. The cache lifetime (300 s default) is a cost-versus-revocation-exposure dial: calls per hot object are 1 per 300 s, so cutting it to the 60 s revocation target multiplies unwraps by 5 for objects read continuously. That is USD 3 per million unwraps, so the cost is small; the dominant risk is rate limits, not money. The DEK-cache policy in M05.02 is **neither confirmed nor revised** until a real provider shows its limits.

## Not done here (all owned by M15.08)

- A bounded run against a real provider with a frozen call, duration and spend envelope, and the raw output beside it.
- Real throttle, denial, timeout and retry-after behaviour, real latency histograms per region, regional and account quota.
- The revisit of the M02.04 and M02.07 envelopes with a real latency number.
- Tests here are characterization tests of existing behaviour and passed on first run; they were not seen red. The meter has unit tests; one mutation (dropping the unwrap counting) was checked and fails the suite.
