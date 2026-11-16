# M05.01 and M05.02: identity, authorization and the key-provider contract

Code: `crates/security` (`hsearch-security`), 66 tests (17 unit, 17 identity, 32 key), 96.2% line coverage (floor 90). Contract summary: [security.md](../security.md#implemented-contracts-m0501-m0502).

## Negative controls (all run, all refused)

Wrong CA; wrong trust domain, cluster and role (including a federated cluster asking for a role it was not federated for); two identities in one certificate; DNS name posing as a role; expired and not-yet-valid certificates inside and outside the 30 s skew; server-only certificate used as a client; empty, truncated, oversized and every-byte-flipped certificates; bearer token to the certificate provider and the reverse; token registered with a peer role; a public token on every peer RPC for every role; a node of another cluster; a replication consumer with a grant on one index pulling another; stale and missing policy; suspended index; non-increasing policy generation; test provider under the production profile; stored key reference off the allowlist (provider call count 0); wrap context mismatch for index, domain and object; every-byte damage to a wrapped key.

A defect the tests found while writing them: registering an existing token a second time replaced its role (a client token could be re-registered as admin). Fixed by checking before inserting; covered by `tokens_identify_clients_and_admins_only`.

## Key-cache call budget (asserted, not estimated)

Create 1 object: 1 wrap. 1,000 reads: 0 provider calls. After the 300 s lifetime: exactly 1 unwrap, then hits. 20 objects read 50 times each: 20 wraps and 20 unwraps. 16 concurrent misses on one key: 1 unwrap. An expired entry with the provider down is an error, not a stale hit. Revoking one index's key and invalidating it blocks that index; the other indexes (own key and default key) keep reading; with no invalidation the cache lifetime bounds the exposure.

## Hot-path numbers (provisional: 4-vCPU VM, release, in-memory test provider, `cargo run --release -p hsearch-security --all-features --example security_bench`)

| Operation | Cost |
|---|---|
| Bearer-token verify (SHA-256, table lookup) | about 100 ns |
| `authorize_api`, one policy of 100,000 held | about 46 ns |
| Data-key cache hit, 1 thread | about 135 ns |
| Cache hit, 4 threads, 64 objects, 16 shards | about 700 ns each, 5.7 M hits/s total |
| Same, 1 shard | about 1,100 ns each, 3.6 M hits/s total |
| Cache hit, 4 threads, one hot key | about 1,400 ns each, 2.9 M hits/s total |
| Cache miss, in-memory provider (AES-256-GCM unwrap + insert) | about 1.5 us |
| `create_object_key` (random key, wrap, insert) | about 2.4 us |
| SPIFFE certificate verify, ECDSA P-256 | about 56 us, once per connection |

## Trade-offs and downsides

- Shards (`CachePolicy::shards`, default 16, 1 for one exact global eviction order): different objects stop contending (3.6 to 5.7 M hits/s at 4 threads), but the entry bound and eviction order hold per shard, so one shard can evict while another has room.
- One hot key read from many threads gets slower per call than one thread (reference-count and lock-word traffic on shared cache lines). It stays about 1.4 us, far under one 64 KiB block decrypt, and a reader opens a key once per object, not per block. Not fixed; a lock-free read path would need a new dependency.
- The cache lifetime (default 300 s) is the worst-case time a revoked key keeps working on a node that has not been told; the plan's 60 s revocation target needs a shorter lifetime or policy-generation delivery, which belongs to M05.05. Shorter lifetimes cost more unwraps (one per object per lifetime).
- Audit records every provider call but not cache hits, to keep the read path free of allocation and locks.
- Memory: at most `max_entries` keys, about 200 B each (20 MB at the default 100,000).
- Dependencies added (exact pins): `rustls-webpki` (ring), `x509-parser`, `aes-gcm`, `zeroize`, `getrandom`, `sha2`; `rcgen` and `time` only under the non-default `dev-ca` feature. `ring` brings C and assembly; the TLS stack chosen later is expected to use the same.

## Not covered

No TLS listener, no policy loading or delivery, no real KMS (M15), no KMS latency or cost per path (M05.08), no ARM run (x86_64 only), no mlock or core-dump controls.

## Independent security review (round 1) and what changed

Fixed: an extended-key-usage-less certificate was accepted for both client and server use (now required, with a test); a cache hit skipped the per-object binding that a cold read enforces (object now in the cache scope); a panic in the provider or audit sink left the key's single-flight entry stuck (drop guard); an unwrap that raced a revocation was cached afterwards (invalidation epoch); expired keys sat in memory until capacity pressure (purged on insert); evicting a policy forgot its generation (bounded floors); any provider denial flushed the whole index's cache, a KMS amplification lever (only a revocation flushes); token labels were unvalidated (now `[A-Za-z0-9._-]`, at most 64); the test KMS is behind the non-default `test-kms` feature.

Left as stated: `Profile` is still a constructor argument, so a deployment that passes `Development` in production defeats the test-provider refusal; deriving it from the build or the deployment record belongs with the node wiring. Tokens have no expiry or per-token revocation (the table is rebuilt to revoke), which the OIDC credential work replaces.
