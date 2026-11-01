# Security, mTLS, and portable BYOK encryption

Status: proposed, requiring crypto-format review and provider qualification. BYOK here means customer-controlled key-encryption keys wrap data-encryption keys. The data path encrypts/decrypts with a vetted AEAD implementation on the node; a KMS call is not made for every data block. This is stronger portability than relying only on a storage provider's SSE format, and introduces application responsibilities for nonce allocation, keys, caches, and revocation.

## Threat model and trust boundaries

Protect tenants from other tenants, unauthorized clients/peers/replication consumers, object substitution/corruption, compromised object credentials without KMS access, accidental plaintext spills, and stale policy or node identities. Bound malicious request/manifest/trace contents. Assume the OS and an authorized running node handling plaintext are trusted. Envelope encryption does not prevent a compromised authorized process from reading its active plaintext or keys. Cryptographic erasure does not prove deletion of copies already exfiltrated.

Separate client, peer, journal, replication, administration and diagnostic audiences. Enforce TLS 1.3/mTLS for node-to-node traffic with role and deployment/trust-domain authorization after certificate validation. A trusted certificate is necessary, not sufficient for every RPC. A query worker cannot reconfigure the journal. Public client tokens cannot be replayed against a peer port. Development uses a generated CA and individual node certificates; never disable verification as the default test path.

SPIFFE X.509 identities are the production identity-provider candidate. File-based certificates remain a supported deployment adapter. Rotate leaf certificates and trust bundles without losing active operations, with a bounded overlap; close/re-authenticate long-lived channels by policy. Test wrong CA, wrong SAN/SPIFFE ID, wrong cluster, expired/not-yet-valid certs, role confusion, old bundles, clock skew and certificate rotation under load. Cross-cluster trust is narrow federation scoped to specific source/export and destination/import roles, not transitive membership. [SPIFFE standard](https://spiffe.io/docs/latest/spiffe-specs/x509-svid/).

Use scoped OIDC/API credentials for clients, source-export grants for CCR/copy, and separate workload credentials for object/KMS providers. Policy changes increment authorization generations; caches have bounded lifetimes and invalidation. Admin/debug reads need dedicated scopes. External object/KMS endpoints are operator-configured allowlists; a tenant cannot turn copy or a key URI into SSRF or an arbitrary credential-bearing request.

## Envelope hierarchy and providers

```text
customer KEK, version pinned at provider
    wraps -> per-object random 256-bit DEK
                 encrypts -> authenticated bounded blocks of WAL/segment/manifest data
object -> immutable authenticated payload + initial provider-wrapped DEK
manifest/policy -> optional immutable rewrap-envelope overlay for that object
```

`KeyProvider` supports `wrap`, `unwrap`, key identity/version discovery and optionally provider-native `rewrap`/`generate_data_key`. A software/test provider uses an isolated test secret and is forbidden in production profiles. AWS KMS, GCP Cloud KMS and Azure Key Vault adapters normalize results without losing provider version/context. Vault transit/HSM/KMIP can be added behind the same contract. Do not promise provider-native rewrap across vendors; fallback unwrap/rewrap occurs only on an authorized node with audit and bounded key lifetime.

Provider key references are tenant policy, not data supplied freely on every query. Validate account/project/vault, key use, algorithm, permissions, residency, and permitted versions before activation. Map encrypt/decrypt permissions separately where the provider permits it. Imported customer key material and externally held keys are provider-specific deployment capabilities; neither is implemented by accepting raw customer secrets in the search API.

Fresh objects receive fresh DEKs from a CSPRNG or provider generation. Plaintext DEKs live only in a bounded, zeroizing cache keyed by tenant, crypto domain, envelope digest, provider key version and policy generation. Do not log, serialize, core-dump or expose key material through diagnostics. Locking memory is a measured defense, not a claim that all allocator/OS copies disappear. Disable core dumps and protect swap for encrypted production profiles.

## Proposed authenticated block format

Ratify a separate versioned format in M05 before production bytes exist. A small bounded outer header carries only routing/decryption necessities: magic/version, suite, immutable object UUID, crypto-domain UUID, bounded plaintext/ciphertext lengths and block count. The initial provider-wrapped DEK is stored inline with bounded authenticated context; a published manifest/policy may select an immutable rewrap overlay later. This avoids a separate key-object PUT for every WAL batch without removing its wrap call. Payload AAD binds immutable object/domain/suite/block/length fields and excludes mutable provider/wrapping-version data; wrapper substitution and overlay policy require separate authentication/context checks. Sensitive metadata, file names, schemas and document values belong in authenticated encrypted content. Object sizes and access patterns remain observable unless a separately costed padding policy is enabled.

AES-256-GCM with a 128-bit tag is the initial suite. Prefer `aes-gcm` or another vetted provider such as aws-lc through a narrow adapter after audit/target qualification. Keep old suites readable during upgrades; do not expose arbitrary algorithm strings as a tenant plugin. Compression is independent per bounded block, before encryption. Decompression verifies an authenticated declared size and expansion budget. [AEAD guidance](https://docs.rs/aes-gcm/latest/aes_gcm/), [range-encryption precedent](https://learn.microsoft.com/en-us/azure/storage/blobs/client-side-encryption).

Each immutable object has one fresh DEK. Use a deterministic 96-bit counter nonce within that key: reserve counter zero for header authentication and use unique counters starting at one for data blocks. Bound objects to a conservative reviewed invocation/byte limit well below counter exhaustion. A retry resends identical ciphertext; any changed plaintext or rebuilt object gets a new UUID and DEK. Crash recovery never resumes a partially encrypted object with an uncertain counter. Do not use global counters or random nonces per block without a quantified collision bound.

AAD binds immutable crypto domain, physical object UUID, suite/format, logical block ordinal, block type, total length and relevant authenticated header digest. It does **not** bind the KEK/envelope version, allowing key rewrap without rewriting payload ciphertext. It does not rely on mutable index names. Cross-domain copy uses fresh object identity/DEK and re-encryption. The core validates that the authorized manifest can reference the crypto domain; it must reject a valid block swapped from another object, tenant, position, or generation.

Authenticate before exposing any plaintext. Bound outer fields before allocation. Corrupt/truncated tags, forged headers, swapped extents, duplicate/missing/reordered blocks, wrong length, wrong envelope, failed unwrap, and unsupported suite all return typed corruption/authorization errors. Range fetches retrieve complete authenticated blocks and enough authenticated metadata, never decrypt a partial GCM record. Initial block-size sweep: 64 KiB, 256 KiB, 1 MiB; final selection is measured against remote request and read amplification.

### Append-only journal encryption and KMS cost

Local quorum logs are append-only segments, not immutable object uploads. If application-level log encryption is enabled, each voter creates a new segment UUID/DEK, durably stores its wrapped envelope before acknowledging any encrypted record, and allocates unique record counters under that segment. After a crash or uncertain counter state, seal the old segment and start with a fresh UUID/DEK; do not resume encryption under its old counter. Existing bytes remain readable with their old envelope. Vote/membership/snapshot metadata has its own reviewed persistence/encryption policy. Physical ciphertext may differ per voter; the consensus application identity/digest remains independent of at-rest encoding.

Archive uploads use new immutable objects and fresh DEKs. KMS wrapping is per object or journal segment, never per scored document, decoded block or log record. Measure request rate/cost for small metadata objects and rollover storms; coalesce transport/metadata where that preserves tenant policy. A more elaborate epoch-key derivation hierarchy requires its own crypto review and revocation/blast-radius model before reducing KMS calls. A performance target cannot be met by silently retaining cached keys beyond their policy lifetime.

Count application BYOK wrapping separately from optional storage SSE-KMS. S3 Bucket Keys affect the S3 service's calls, not hsearch's `KeyProvider`. Packing is initially within a compatible tenant/crypto/residency policy; a physical pack containing unrelated tenants would need separately authenticated/encrypted extents and its own review. Cost optimization cannot substitute one shared key or invalidate independent revocation. [Cost and KMS research](research/cost-economics.md#byok-and-storage-classes-can-reverse-apparent-savings) defines the accounting requirement.

## Rotation and revocation

**KEK rotation** changes which provider key version wraps a DEK. Write a new immutable envelope overlay, verify it can be unwrapped, atomically publish a manifest/policy selecting it, then retain old dependencies until every PIT/snapshot/replica/copy/CCR view has migrated or expired. The original inline wrapper remains immutable, but cannot bypass current authorization/revocation policy. This changes envelope overhead, not payload ciphertext. Maintain an inventory of old-version roots; no key version may be retired merely because new writes use a new one.

**DEK/cipher rotation** creates new ciphertext objects and manifests through compaction/migration. It requires full data I/O and new nonces. Keep old readers working until their pins end. Rewrapping alone does not change plaintext, cipher, or tenant isolation.

**Revocation** is an enforced policy event with a declared maximum cache/channel/reader lifetime, not an instant mathematical guarantee. Proposed service target: refuse new tenant operations immediately after a locally accepted revocation generation; reach every healthy node and invalidate DEKs/plaintext reader caches within 60 seconds. If policy freshness cannot be proven by its deadline, fail closed. Operations already holding plaintext are cancelled/drained where possible; memory zeroization and process eviction are separate controls. Report provider propagation delays and partition limits rather than claim a global 60-second guarantee without measurement.

KMS outage: existing cached DEKs may be used only until the configured short TTL and authorization freshness expire. No refresh-on-error or indefinite stale-key fallback. New object writes fail when wrapping cannot complete. Key deletion/revocation must make recovery fail visibly; restoring a bucket is insufficient if the necessary key versions have been destroyed. Include keys/grants/policy in disaster-recovery runbooks while keeping backups separate from plaintext keys. [AWS data-key lifecycle](https://docs.aws.amazon.com/kms/latest/developerguide/data-keys.html).

## Local storage and hardware acceleration

Encrypt durable journal records, object payloads, persistent cache, replay spool and index scratch under the applicable tenant/storage policy. The current engine expects ordinary filesystem bytes for writes/mmap; application object encryption alone would leave plaintext local files. The initial production profile therefore requires encrypted scratch/journal/cache volumes plus restricted permissions and verified cleanup, or a separately qualified encrypted engine-directory implementation. WSL/Docker emulation cannot certify that production volumes are encrypted. Report local-volume encryption capability in readiness.

Use runtime AES/PMULL/PCLMUL dispatch from the chosen crypto backend. Qualify x86_64 AES-NI and aarch64 AES/PMULL paths, software fallback on supported CPUs, and force-disabled feature tests. VAES is optional after measurement; do not compile a generic release with `target-cpu=native`. Expose the selected backend/feature set without secrets. Hardware acceleration does not justify custom cryptography or an unsafe shortcut past tag verification. [Backend documentation](https://docs.rs/aes/latest/aes/).

## Release evidence

M05/M15 require known-answer vectors, independent decrypt interoperability, every-byte corruption/truncation controls, nonce-reuse negative controls, restart/retry identity checks, cross-tenant swap failures, KMS throttling/denial/outage, rewrap/old-reader tests, cached-key revocation, encrypted spill/volume inspection, and cross-cluster rekey. Measure encrypted versus plaintext baseline CPU, throughput, allocations, remote bytes and cold/warm tail latency on real x86_64 and ARM hardware. Security review must inspect the format and lifecycle, not only a library's AES implementation.
