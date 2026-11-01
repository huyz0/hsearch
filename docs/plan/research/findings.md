# Research findings and architectural judgment

Research date: 1 November 2026. These are design recommendations derived from primary documentation and the [repository audit](repository-audit.md). All hsearch performance numbers are proposed qualification targets or model inputs until measured. Public vendor behavior is evidence about that vendor, not an implementation guarantee available by imitation.

## What the comparable systems teach

| System | Evidence and useful mechanism | Fit and limits for hsearch |
| --- | --- | --- |
| Lucene Rust | Existing Rust indexing, search, codecs, Java differential fixtures | Required engine. Remote reads, stable API, and mutation/visibility integration need direct contract work |
| pstore | Object-only durable tier; lanes, group commit, publication, tiered caches, resource accounting | Reuse ideas and proven adapters selectively. Its vector-centric indexing and partially superseded metadata designs cannot be adopted wholesale |
| OpenSearch fork | Object-register authority today; planned SSD quorum journal and separate fresh replicas | Closest semantic reference for OpenSearch replacement. Rust implementation must prove the same speculative-state and checkpoint boundaries |
| [turbopuffer architecture](https://turbopuffer.com/docs/architecture) | WAL on object storage, asynchronous indexing, memory/NVMe caches, routing affinity | Strong evidence for compute/storage separation and cold-read design. Vendor latency examples are workload-specific |
| [turbopuffer guarantees](https://turbopuffer.com/docs/guarantees) | Durable object-store writes, conditional batches, per-namespace sharding | More specific visibility caveats qualify the architecture page's broad consistency statement. Define hsearch freshness precisely instead of copying a label |
| [Quickwit](https://quickwit.io/docs/main-branch/overview/architecture) | Immutable splits, opening metadata in hotcache, search/index separation, rendezvous cache affinity | Strong fit for immutable search units and pruning. Cluster metastore and append-oriented assumptions are different from a mutable OpenSearch replacement |
| [OpenSearch remote-backed storage](https://docs.opensearch.org/latest/tuning-your-cluster/availability-and-recovery/remote-store/index/) | Primary indexing, remote translog, segment replication, primary-term validation | Reuse committed segment publication and fencing concepts. It remains an OpenSearch cluster feature, not proof that the cluster-manager role disappears |
| [SlateDB](https://slatedb.io/) | Object-store LSM, manifest fencing, independent readers | Study metadata, read snapshots, GC boundaries. A second LSM is not needed merely to store Lucene files |
| [SlateDB GC](https://slatedb.io/docs/design/gc/) | Durable GC boundaries, active-root retention, replication-aware deletion filtering | Important evidence that an age threshold alone cannot exclude a suspended stale publisher |
| [Vespa Proton](https://docs.vespa.ai/en/content/proton.html) | Transaction log before mutation, bounded write concurrency, separately searchable copies | Useful latency and admission model. Its content-node ownership model is not object-store-only |
| [Polars](https://github.com/pola-rs/polars/blob/main/README.md) | Vectorized execution, bounded streaming and I/O/expression extensions | Useful performance engineering reference. Its dataframe execution model is not a distributed search durability protocol |

No one design supplies all requirements. The recommended system combines Lucene's semantics with immutable remote segments, explicit ordered commits, and cache-aware serving. Vector/hybrid search should use the engine's audited capabilities through a query module rather than introduce another index engine by default.

## Durability alternatives

| Candidate | Write acknowledgement | Merits | Costs and failure boundaries | Decision |
| --- | --- | --- | --- | --- |
| A. Object WAL with fenced ordered slots | Conditional create of the next authorized lane slot, plus separately amortized structural checkpoints | One durable service; no per-batch head CAS; recoverable after compute disks disappear | Requires serialized appends, poison-on-failure, retained fences/tail recovery and paid latest barriers | Selected first backend after the [completed cost research](cost-decisions.md) |
| B. Replicated SSD journal plus async object archive | Majority fsync, optionally wait for committed application/refresh | Low write latency; independent query fleet; no object round trip per write | Persistent voter fleet, membership/recovery complexity; archive lag is disaster-recovery exposure | Required low-latency module after A, not a renamed A |
| C. Kafka/Redpanda or managed durable stream | External broker commit | Mature log retention and consumer offsets | Another operational dependency; client/partition semantics do not solve engine publication or authorization | Future `CommitLog` adapter if an operator already uses one |
| D. Active-active writers with CRDT/LWW conflicts | Independent regional commits | Regional write availability | Changes conditional updates, order, versions, deletes, and scripts; incompatible with expected OpenSearch semantics without a new product contract | Excluded from initial release, explicit later RFC |
| E. Full OpenSearch/JVM shell with native FFI engine | Existing shell's chosen durability mode | Fastest route to broad existing API/plugin reuse | Retains the runtime and substantial OpenSearch coupling the new Rust store intends to replace | Differential reference and migration bridge, not hsearch core |

Choose A before B to establish a smaller correctness surface and a complete end-to-end RustFS path. Do not let A become a permanent substitute for the low-latency track. Both implement the same resolved-operation and checkpoint contracts, but a deployment/index activation uses exactly one authority. Mode changes require a quiesced, fenced migration with a recovery checkpoint. “Persist in object storage” does not imply that B's unarchived acknowledged tail already survives loss of every journal disk.

## Metadata choices

Object-native metadata avoids a mandatory database and matches the user's prior projects. Use sharded catalogs and structural authority/checkpoint heads for compatible tenant domains; ordinary WAL batches commit through ordered conditional-create slots. The [executed cost research](cost-decisions.md) selected this direction after inspecting the fork's fencing implementation and testing small race/batching models. The former per-batch root-CAS path remains a comparison protocol. Cross-shard API operations return per-item outcomes unless a separate transaction contract exists. Never imply atomic bulk across shards.

Avoid a global HEAD, a per-document CAS, and global tenant manifests that serialize every hot tenant. Avoid a heartbeat PUT for each of millions of idle shards. A shard coordinator can serialize local proposals to reduce contention, but correctness comes from conditional publication and epoch transitions, not from routing affinity or local mutexes. Catalog pages are enumeration indexes; they are not alternate write authorities.

Cost selection is constrained by profile-specific latency/freshness and failure guarantees. [Operating profiles](../operating-profiles.md) distinguish economical object durability, resilient serving cells and zonal versus multi-AZ quorum. No cost controller can silently downgrade an activated profile.

An external metadata adapter can be added later for deployments needing transactional catalog operations. Its contract must preserve epochs, publication ordering, pin/GC coordination, and failure outcomes. Choosing PostgreSQL or etcd would not remove those proof obligations. Do not ship two metadata authorities for the same state.

## Object storage is a semantic dependency

S3 supports conditional create/update; GCS uses generation preconditions; Azure supports ETag-based concurrency. They expose different version tokens and error handling. An ETag is opaque, not a portable checksum. Preserve raw version tokens, use strong hashes separately, pin range reads to immutable identities, and distinguish a definitive precondition failure from a timeout after a possible success. [S3](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html), [GCS](https://docs.cloud.google.com/storage/docs/request-preconditions), [Azure](https://learn.microsoft.com/en-us/azure/storage/blobs/concurrency-manage).

RustFS documents a tested subset of S3 and warns that server-side encrypted object formats are not portable across implementations. Its compatibility matrix is not evidence that every conditional-write race required here passes. Run the hsearch capability and linearizability probe against the exact image digest and topology; fail startup for unsupported required operations. Filesystem/in-memory fakes prove algorithm properties, not a cloud provider's behavior. [RustFS compatibility](https://docs.rustfs.com/en/reference/s3-compatibility).

Prefer `object_store` as the initial cloud API adapter because its current API exposes conditional put and cloud backends directly. Compare OpenDAL at the required semantics boundary, particularly cancellation, retries, range versioning, and conditional-update outcomes. Do not maintain both without a demonstrated use case. Foyer is the candidate RAM/NVMe cache, behind a narrow adapter; caches remain disposable. The locally inspected foyer OpenDAL extension explicitly lacks restart recovery/shared-writer support and is not authoritative storage. [object_store](https://docs.rs/object_store/latest/object_store/), [foyer](https://github.com/foyer-rs/foyer).

## Extensibility without losing invariants

Use typed Rust traits for storage, logs, identity, key wrapping, caches, query/ingest extensions, snapshots, and export/change streams. Compile trusted performance-sensitive modules together. Use versioned RPC boundaries for separately deployed or untrusted extensions. A dynamic Rust shared-library ABI is unsuitable as the default plugin contract because compiler/type layout compatibility is not a stable distribution boundary.

Do not make “how to commit”, fencing, auth checks, nonce allocation, GC eligibility, or checkpoint interpretation arbitrary callbacks. Modules contribute capabilities and retention pins; the core validates and enforces their invariants. Every provider gets a conformance suite, bounded resources, cancellation, fault behavior, version compatibility, observability, and unload rules. [Module catalog](../modules.md) defines the concrete seams.

## Replication and encryption findings

OpenSearch CCR uses active-passive follower pulls and explicit pause/resume/stop. Retention expiry requires snapshot bootstrap again; it is not solved by retrying the old offset. hsearch should share snapshot/change-stream primitives between CCR and one-time copy while maintaining separate controllers and credentials. [CCR](https://docs.opensearch.org/latest/tuning-your-cluster/replication-plugin/index/), [retention expiry](https://docs.opensearch.org/latest/tuning-your-cluster/replication-plugin/force-resume/).

turbopuffer distinguishes copy-on-write branching from materialized copy and documents encryption inheritance on branches. Cross-cluster copy must produce an independent destination; deleting the source or losing its KMS permissions must not break the destination. A metadata-only clone within one store has different GC and key dependencies. [Branching](https://turbopuffer.com/docs/branching), [encryption](https://turbopuffer.com/docs/encryption).

Use portable application envelope encryption for hsearch's requested pluggable BYOK: a provider wraps DEKs while node-local AEAD encrypts blocks. This differs from turbopuffer's documented cloud-managed encryption. Existing cloud SSE can remain another defense. AES-GCM has hardware-backed implementations on target CPUs, but actual acceleration depends on the selected library/backend and CPU capability. Test dispatch and throughput, never infer them from the architecture name. [AWS data keys](https://docs.aws.amazon.com/kms/latest/developerguide/data-keys.html), [AES-GCM](https://docs.rs/aes-gcm/latest/aes_gcm/), [AES backend dispatch](https://docs.rs/aes/latest/aes/).

## Choices intentionally left to measured spikes

1. Remote-input engine API and its upstream adoption. This is the first architecture gate because all cold-read targets depend on it.
2. Block/prefetch sizes, refresh batching, compression, cache admission, and object packing. Choose from request traces and a cold/warm matrix, not vendor latency anecdotes.
3. OpenRaft versus raft-rs for the Rust quorum module. The candidate must meet real persistence, membership, mTLS, and per-group resource gates. A library's example memory store is not a durable journal.
4. Number of physical log groups, tenant co-location, and hot-shard splitting. Simulate the target catalog and qualify actual deployment sizes separately.
5. Crypto backend selection, FIPS-required build if requested later, and VAES/SVE/io_uring/QUIC/GPU work. Benchmark benefits against supported baseline paths and total-system cost.

These decisions have bounded tasks and rejection criteria in the roadmap. They do not block authoring the plan or the independent harness/storage contracts.

## Polars-style optimization: what to adopt

The inspected CSV line-splitting kernel has a scalar path, an optional SIMD path, cached lane masks and unchecked slice operations inside its hot loop. This is a concrete example of specializing a measured kernel while keeping a reference path. It is not evidence that an unsafe block elsewhere is safe. For hsearch, require boundary/alignment/tail tests and a local proof for each unchecked access, isolate it behind a safe adapter, and compare service-level cost before accepting it. Do not copy another project's nightly feature configuration without checking the pinned Rust toolchain. [Inspected kernel](https://github.com/pola-rs/polars/blob/main/crates/polars-io/src/csv/read/parser.rs) (file blob `6e4e415552958eb1584d133d41ace2a44856b9b6`).
