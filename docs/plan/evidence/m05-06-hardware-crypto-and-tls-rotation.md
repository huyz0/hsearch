# M05.06: hardware crypto reporting and runtime TLS rotation

Code: `crates/security/src/backend.rs` (`report`), `crates/security/src/tls.rs` (feature `tls`: `RotatingCert`, `PeerTls`, `PeerSession`), `RotateError`, `Minted::key_pkcs8` in the dev CA. Tests: `crates/security/tests/backend.rs` (4), `crates/security/tests/tls.rs` (9), one stalled-provider test in `crates/security/tests/keys.rs`. CI step "Security crate on the forced software AES path" runs the whole crate with the software switches.

## Hardware and software compute the same bytes (measured here, x86_64 only)

The cipher is the vetted `aes` and `ghash` crates; no cryptography was written. `backend::report()` states what runs: the AES answer is `aes::hardware_accelerated()`, GHASH repeats the feature test its backend uses (the dispatch is not exposed). On the machine used (Intel Xeon 2.1 GHz, 4 vCPU, AES-NI, PCLMULQDQ, VAES; a shared VM, so provisional):

- default build: `arch=x86_64 aes=hardware ghash=hardware forced_soft=false`
- `RUSTFLAGS='--cfg aes_backend="soft" --cfg polyval_backend="soft"'`: `aes=soft ghash=soft forced_soft=true`

`tests/backend.rs` holds AES-256-GCM to digests computed by Python's `cryptography` (OpenSSL) at 14 lengths from 0 to 65,537 bytes, round-trips, and rejects every one of 448 single-bit flips of a 40-byte message and a wrong AAD. It passes on both paths. The whole crate (`--all-features`: 17+4+21+17+33+10+19+9 tests, including the independent block-format vector from `block_v1.py`) also passes on the forced-software build, so the sealed format is byte-identical on both. A test asserts the report never claims hardware where the CPU lacks the feature, and that a forced build reports software.

A CPU without the instructions falls back to software inside the crates; an unsupported instruction is not executed. There is no plaintext path and no way to skip tag verification: the backend choice never touches the verify call.

### Throughput, hardware against software (`block_bench`, release, 64 MiB object; provisional, shared VM)

| Block size | Seal MB/s hw | Seal MB/s soft | Open all MB/s hw | Open all MB/s soft |
|---|---|---|---|---|
| 4 KiB | 978 | 98 | 1020 | 101 |
| 64 KiB | 1091 | 103 | 1102 | 105 |
| 1 MiB | 1090 | 113 | 1079 | 112 |

The software path is about 10x slower: a node silently on it is a capacity problem, which is why the backend is reported rather than hidden. Not measured: VAES against AES-NI, aarch64 (the detection code type-checks for aarch64; it was not built into a binary or run, and the manual ARM workflow has not run this suite), memory use.

## Runtime TLS rotation (real TCP, rustls over ring, TLS 1.3 only)

Design: each `ServerConfig` and `ClientConfig` is built once with a certificate resolver (`RotatingCert`) and a peer verifier that read current state at each handshake, so a rotation never rebuilds a config and never touches a connection that finished its handshake. Chain validation is the existing `ChainVerifier` through the `IdentityProvider`, so role, trust domain, cluster and bundle overlap apply during the handshake. Rotation is verified before installing: a certificate the node's own verifier would refuse, a key that does not match, or a bad chain is refused and the old certificate stays. Long-lived channels are re-authenticated by `PeerSession::check` (age bound, certificate expiry, issuer left the bundle after its overlap, clock reversal).

Findings while writing the tests (both fixed, both kept as tests):

- **TLS session resumption skips certificate verification.** With default rustls settings a reconnect after a rotation still presented the old certificate, and a connection under a retired CA was accepted after its overlap ended. Resumption is disabled on both configs, so every connection does a full handshake. Cost: one full handshake per connection, none per request. Not measured here.
- The handshake clock is whole seconds, so a bundle overlap shorter than a second is not precise; the overlap test uses 1 s.

Tests (`tests/tls.rs`, run three times in a row, all green): mutual handshake is TLS 1.3 and names the peer's role; the server certificate rotates while a stream is open (open stream continues and keeps the old identity, next connection sees the new one); the client certificate rotates; refused rotations (untrusted CA, expired, key mismatch, empty or oversized chain, wrong role) leave generation and leaf unchanged; trust bundle rotation (old-CA certificates work during the overlap, new handshakes are refused after it, the open stream keeps carrying bytes, `check` says it must end, re-issuing under the new CA resumes service); session ends by age and by clock reversal and by certificate expiry; **20 rotations of both certificates under 6 concurrent streams (at least 50 round trips each, payloads up to 70 kB) with a fresh handshake after each, no byte lost or corrupted**; a TLS 1.2-only client and an anonymous TLS 1.3 client cannot connect, and a client trusting another CA refuses the server.

## Stalled unwrap budget

`a_stalled_unwrap_is_bounded_by_the_call_budget_and_does_not_block_warm_keys`: with the provider stalled for 30 s and a 250 ms budget, eight concurrent readers of one cold object all return `Timeout` within 4x the budget, make exactly one provider call between them, a warm key is served in under 100 ms during the stall, and the next read after the provider recovers succeeds (nothing cached from the failure).

## Not claimed

- No ARM run, no VAES measurement, no hardware difference on a bare-metal host.
- TLS is exercised through a test echo protocol, not the node's peer port; wiring the listener into the node is a later milestone. `PeerSession::check` is a function the caller must call between messages; nothing here schedules it.
- Private keys inside `rustls`' signing key are not under this crate's zeroization.
- The `tls` feature is not enabled by any production crate yet.
