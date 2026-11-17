# M05.07: security format review

Scope: the encrypted block format, key lifecycle, identity and TLS from M05.01 to M05.06, reviewed against `security.md`'s threat model. The cloud providers are **not** qualified here: every real-provider claim stays pending M15 (and M05.08's real-provider run, which is blocked).

## Threat model against evidence

| Threat in security.md | Held by | Status |
|---|---|---|
| Tenant reads another tenant's objects (swap, substitution) | Context-bound wrap (index, domain, object), AAD binds object, domain, block ordinal and length; `tests/block.rs` every-byte and swap controls, `tests/adversarial.rs` block swaps and duplicates | Tested |
| Corrupt, truncated or forged object returns plaintext | Header tag then block tags before any plaintext; `adversarial.rs`: every truncation and extension, every header byte at five boundary values, three length fields at five extremes, 20,000 seeded garbage inputs (half with the real magic) and 20,000 seeded 1 to 4 byte mutations, all refused, header allocation never above `MAX_HEADER_LEN` | Tested; a fixed-seed corpus, not a fuzzer run |
| Nonce reuse | Deterministic counter nonces, one fresh key per object, retry resends identical bytes; M05.03 controls | Tested; "fresh key per object" is a caller obligation the API cannot check (stated in M05.03) |
| Compromised object credentials without KMS access | Object bytes are ciphertext; wrapped key needs the provider | By construction; not tested against a real provider |
| Stale policy, revoked key still served | `PolicyGate` fails closed; cache lifetime now **capped at 60 s under the production profile** (finding 2) | Tested |
| KMS outage or throttling | No stale fallback; optional failure backoff (finding 1) | Tested |
| Wrong peer, role confusion, expired or untrusted certificates | `ChainVerifier` through the identity provider on both TLS sides; handshake tests for rogue CA, wrong role, no certificate, TLS 1.2 | Tested (M05.01, M05.06) |
| Plaintext spill outside object data | Readiness refuses production with any unprotected path (M05.04) | Tested as a predicate; real volume probes are M15 |
| Provider claims | `conformance::run` is the contract as code; the test provider passes 13 of 13 checks and each of two deliberately broken providers fails the check that names its rule; a provider that cannot wrap conforms to nothing | **Real providers: not run (M15)** |

## Findings and what was done

1. **A failing provider is called once per attempt** (found by M05.08's counts: 100 refused reads are 100 calls). Added `KeyManager::with_failure_backoff`: a retryable failure (throttled or unavailable; never a timeout, which is one caller's own budget and was found by the review to poison the key for callers with longer ones) is remembered per customer key for the setting, up to 30 s, and a throttle's retry-after is honoured up to that cap. A denial or revocation is never remembered. With 20 cold objects and 1,000 failing reads, one provider call is made instead of 1,000. **Off by default**: the downside (a recovered provider is not asked again for up to the window) is bounded but real, and no node wires the manager yet, so there is nothing to measure it against; whoever wires the node sets it from the provider's rate limit. Existing tests pin the off behaviour; new tests cover the on behaviour. Not seen red before the code existed (written together).
2. **Nothing enforced the relation between the data-key cache lifetime (300 s default) and the 60 s revocation target** (M05.05 noted it as a caller obligation). The production profile now refuses a lifetime above 60 s (`MAX_PRODUCTION_TTL`); development keeps 300 s. Cost: unwraps for a continuously read object rise from 1 per 300 s to 1 per 60 s, which at the rate card is USD 3 per million unwraps, so money is not the constraint; the provider's rate limit is (unmeasured, M05.08).
3. **AES-GCM is not key-committing.** Per-object keys from the manager plus a provider that checks context make a key-substitution attack unreachable, and the header tag only verifies under the unwrapped key. Left as documented (M05.03); revisit if keys ever come from anywhere but the manager.
4. **The TLS verifier does not match server names and has no revocation or OCSP check** (M05.06 evidence): identity is role and trust domain, lifetime is certificate validity and the bundle. Acceptable for the stated model; a revoked node certificate stays valid until it expires or the bundle rotates.
5. **Private key bytes pass through one non-zeroized copy into rustls** (M05.06). Accepted and stated.
6. **No fuzzer run.** The corpus is deterministic and seeded; a cargo-fuzz campaign is not claimed.

No finding blocks the durable-store release; findings 1 and 2 were fixed here, 3 to 6 are recorded limits.

## Provider conformance report (test provider)

```
provider conformance: test
  pass     wrap names the key it used
  pass     wrapped bytes do not contain the data key
  pass     unwrap returns the same data key
  pass     key_info reports a usable key and a version
  pass     unwrap for another index is refused
  pass     unwrap for another domain is refused
  pass     unwrap for another object is refused
  pass     damaged wrapped bytes never unwrap
  pass     an unknown key is not found
  pass     a cancelled call is cancelled
  pass     errors name a code and no key material
  pass     a revoked key refuses wrap and unwrap, and restore recovers
  pass     rotation keeps old wraps readable and new wraps use the new version
```

Run without owner actions, the last two report `NOT-RUN` and `fully_conforms()` is false (that, not `conforms()`, is the qualification bar). A provider that cannot wrap fails the first check and stops. The revoke and rotate checks assume immediate effect, so a real run must wait for propagation inside its owner actions. Checks that need a real account (throttle thresholds, denial by policy, key destruction, regional behaviour) are not in the suite yet: M15 adds them with the adapters.

## Encrypted-path acceptance artifacts

[M05.03](m05-03-encrypted-block-format.md) (format and independent vector), [M05.04](m05-04-persistent-path-encryption.md) (sealed backend, spill inventory, readiness), [M05.05](m05-05-rotation-and-revocation.md) (rotation, revocation, shredding, outage), [M05.06](m05-06-hardware-crypto-and-tls-rotation.md) (backend, TLS), [M05.08](m05-08-kms-path-accounting.md) (call counts).

## Coverage

`cargo llvm-cov --all-features --package hsearch-security` on this branch: **95.18% lines** (3,482 lines, 168 missed), 94.41% regions; floor 90%. Per file the lowest are `backend.rs` 83.7% (the aarch64 and non-x86 detection branches cannot run here), `conformance.rs` 88.9% and `tls.rs` 89.0%.

## Independence

The author's own review of the threat table is not independent. A separate reviewer session examined the commit; it found no P0 or P1 and two P2s (a timeout remembered across callers; the independence claim recorded before the review ran), both fixed in this commit, plus P3 documentation fixes. Its report is in the PR thread, not a file in the tree. The production-cache-lifetime refusal now has its own error.
