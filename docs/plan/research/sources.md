# Primary source register

Accessed 1 November 2026. Repository commits and inspected files are recorded in [repository-audit.md](repository-audit.md). Public documentation URLs are living sources; re-check them when pinning an implementation release. Read-only source inspection was performed; no source project's test suite was run. Brief findings below are paraphrases. The research uses primary documentation and source code, not third-party benchmark rankings.

| ID | Source | What it supports; limits |
| --- | --- | --- |
| S01 | [Lucene Rust workspace](https://github.com/huyz0/lucene-rust/blob/46e280bc2a208544fb36ecd95932e89cd8bc5618/Cargo.toml) | Crate inventory and dependencies; no released API promise |
| S02 | [Lucene Rust directory](https://github.com/huyz0/lucene-rust/blob/46e280bc2a208544fb36ecd95932e89cd8bc5618/crates/lucene-store/src/directory.rs) | Whole-file Input and filesystem output coupling |
| S03 | [Concurrent writer](https://github.com/huyz0/lucene-rust/blob/46e280bc2a208544fb36ecd95932e89cd8bc5618/crates/lucene-index/src/concurrent_writer.rs) | Commit cuts, indexing slots, stated unsupported combinations |
| S04 | [Lucene AI instructions](https://github.com/huyz0/lucene-rust/blob/46e280bc2a208544fb36ecd95932e89cd8bc5618/AGENTS.md) | Fixtures, coverage caveats, container constraints, navigation |
| S05 | [Lucene CI](https://github.com/huyz0/lucene-rust/blob/46e280bc2a208544fb36ecd95932e89cd8bc5618/docs/ci.md) | Conditional jobs, nextest, cache lessons; historical results only |
| S06 | [Lucene code review](https://github.com/huyz0/lucene-rust/blob/46e280bc2a208544fb36ecd95932e89cd8bc5618/.agents/skills/code-review/SKILL.md) | Deterministic versus semantic review boundaries |
| S07 | [pstore architecture](https://github.com/huyz0/pstore/blob/6ddaf56dd0e34862561b63b173ac3de5cfb50fb4/docs/research/11-design/architecture.md) | Object-native design ideas; internal drift documented in audit |
| S08 | [pstore WAL](https://github.com/huyz0/pstore/blob/6ddaf56dd0e34862561b63b173ac3de5cfb50fb4/docs/research/05-storage-engine/write-path-and-wal.md) | Lanes and batching; explicitly partly superseded |
| S09 | [pstore harness](https://github.com/huyz0/pstore/blob/6ddaf56dd0e34862561b63b173ac3de5cfb50fb4/docs/research/09-rust-stack/agent-harness.md) | Review-loop/token failure modes |
| S10 | [pstore peer protocol](https://github.com/huyz0/pstore/blob/6ddaf56dd0e34862561b63b173ac3de5cfb50fb4/docs/milestones/M127/ARCHITECTURE.md) | Admission, framing, deadline and identity contracts |
| S11 | [OpenSearch fork shell](https://github.com/huyz0/OpenSearch/blob/ad254f6439dc4e93f5f00461b85a086e53a3d2da/serverless/README.md) | Current object-WAL shell and integration surface |
| S12 | [Fork low-latency plan](https://github.com/huyz0/OpenSearch/blob/ad254f6439dc4e93f5f00461b85a086e53a3d2da/serverless/low-latency-serverless-plan.md) | Proposed quorum/archive/freshness model |
| S13 | [Fork progress](https://github.com/huyz0/OpenSearch/blob/ad254f6439dc4e93f5f00461b85a086e53a3d2da/serverless/low-latency-progress.md) | Which foundations exist; not production quorum completion |
| S14 | [Fork consensus ADR](https://github.com/huyz0/OpenSearch/blob/ad254f6439dc4e93f5f00461b85a086e53a3d2da/serverless/low-latency-consensus-adr.md) | Physical-group footprint and committed-serving boundary |
| S15 | [Fork takeover RFC](https://github.com/huyz0/OpenSearch/blob/ad254f6439dc4e93f5f00461b85a086e53a3d2da/serverless/rfc-wal-first-takeover.md) | Why blind log-first ack changes OpenSearch semantics |
| S16 | [lsp-cli](https://github.com/huyz0/lsp-cli/blob/cd9d492f950ab8fb8a9f54e4aea2f72cfd1aab48/README.md) | Compiler navigation, daemon/schema, Linux/macOS limits |
| S17 | [autoreview](https://github.com/huyz0/autoreview/blob/6b1a756cb8ac828a68faaf138cbf71a40f42bcf2/README.md) | Deterministic-first review; optional analyzer limitations |
| S18 | [turbopuffer architecture](https://turbopuffer.com/docs/architecture) | Object WAL, caching, indexing separation |
| S19 | [turbopuffer guarantees](https://turbopuffer.com/docs/guarantees) | Specific durability/visibility and sharding qualifications |
| S20 | [turbopuffer encryption](https://turbopuffer.com/docs/encryption) | SSE-based CMEK, revocation/rotation, copy and branching distinction |
| S21 | [turbopuffer branching](https://turbopuffer.com/docs/branching) | Independent logical branches backed by shared objects |
| S22 | [turbopuffer write API](https://turbopuffer.com/docs/write) | Copy/mutation API ideas; not an hsearch protocol |
| S23 | [turbopuffer sharding](https://turbopuffer.com/docs/sharding) | Shared WAL versus parallel indexes |
| S24 | [Quickwit architecture](https://quickwit.io/docs/main-branch/overview/architecture) | Splits, hotcache, metastore and cache affinity |
| S25 | [OpenSearch remote storage](https://docs.opensearch.org/latest/tuning-your-cluster/availability-and-recovery/remote-store/index/) | Remote translog/segments and primary validation |
| S26 | [OpenSearch CCR](https://docs.opensearch.org/latest/tuning-your-cluster/replication-plugin/index/) | Pull-based active-passive replication |
| S27 | [CCR force resume](https://docs.opensearch.org/latest/tuning-your-cluster/replication-plugin/force-resume/) | Retention expiry and rebootstrap |
| S28 | [OpenSearch restore](https://docs.opensearch.org/latest/api-reference/snapshots/restore-snapshot/) | Restore namespace/identity conflict handling |
| S29 | [OpenSearch Serverless encryption](https://docs.aws.amazon.com/opensearch-service/latest/developerguide/serverless-encryption.html) | KMS-managed at-rest encryption policies |
| S30 | [SlateDB](https://slatedb.io/) | Object-store LSM and manifest-fencing design |
| S31 | [SlateDB files](https://slatedb.io/docs/design/files/) | Separate manifest, WAL and compacted objects |
| S32 | [SlateDB GC](https://slatedb.io/docs/design/gc/) | Boundaries, live roots and replication filters |
| S33 | [Vespa Proton](https://docs.vespa.ai/en/content/proton.html) | Log-before-mutation, bounded persistence and searchable copies |
| S34 | [RustFS compatibility](https://docs.rustfs.com/en/reference/s3-compatibility) | Subset guarantee and encryption portability warning |
| S35 | [S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html) | If-Match/create, conflict and multipart retry cases |
| S36 | [GCS preconditions](https://docs.cloud.google.com/storage/docs/request-preconditions) | Generation-based identity and conditional operations |
| S37 | [Azure concurrency](https://learn.microsoft.com/en-us/azure/storage/blobs/concurrency-manage) | ETag concurrency control |
| S38 | [Rust object_store](https://docs.rs/object_store/latest/object_store/) | Conditional put and provider abstraction |
| S39 | [AWS data keys](https://docs.aws.amazon.com/kms/latest/developerguide/data-keys.html) | Wrapping versus local payload encryption |
| S40 | [Azure client encryption](https://learn.microsoft.com/en-us/azure/storage/blobs/client-side-encryption) | Envelope and authenticated range-block design precedent |
| S41 | [RustCrypto AES-GCM](https://docs.rs/aes-gcm/latest/aes_gcm/) | AEAD security notes and nonce requirements |
| S42 | [RustCrypto AES backends](https://docs.rs/aes/latest/aes/) | Runtime x86/ARM acceleration and unsupported-CPU caveat |
| S43 | [SPIFFE X.509 SVID](https://spiffe.io/docs/latest/spiffe-specs/x509-svid/) | Workload identities and validation rules |
| S44 | [OpenTelemetry sensitive data](https://opentelemetry.io/docs/security/handling-sensitive-data/) | Collection minimization/redaction; application policy still required |
| S45 | [OpenRaft integration](https://docs.rs/openraft/latest/openraft/docs/getting_started/index.html) | Storage/network/state-machine obligations; examples are not production storage |
| S46 | [Turmoil](https://docs.rs/turmoil/latest/turmoil/) | Deterministic network simulation; unstable filesystem features require evaluation |
| S47 | [Loom](https://docs.rs/loom/latest/loom/) | Small concurrency models and instrumented primitives |
| S48 | [nextest partitioning](https://nexte.st/docs/ci-features/partitioning/) | Test sharding and partition control |
| S49 | [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) | Clean/merge coverage across test conditions |
| S50 | [GitHub workflow syntax](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax) | Job dependencies, concurrency, workflow triggers |
| S51 | [Rust target features](https://rust-lang.github.io/rfcs/2045-target-feature.html) | Dispatch safety background; verify current compiler before implementation |
| S52 | [Codex worktrees](https://learn.chatgpt.com/docs/environments/git-worktrees) | Official worktree behavior; hsearch adds its own resource isolation |
| S53 | [Codex custom review rules](https://developers.openai.com/blog/custom-code-review-rules-for-codex) | Concise repository-scoped review guidance |
| S54 | [Polars README](https://github.com/pola-rs/polars/blob/main/README.md) | Vectorized/streaming execution and extension ideas; accessed file blob `081f4554d80229dc46d2b4a17c6ec738a75be11d` |
| S55 | [Polars CSV kernel](https://github.com/pola-rs/polars/blob/main/crates/polars-io/src/csv/read/parser.rs) | Inspected scalar/SIMD/unchecked-slice implementation excerpt; file blob `6e4e415552958eb1584d133d41ace2a44856b9b6` |
| S56 | [S3 pricing](https://aws.amazon.com/s3/pricing/) | Request/storage-class/transfer rules and examples; dynamic regional rates require an exported rate card |
| S57 | [S3 gateway endpoints](https://docs.aws.amazon.com/vpc/latest/privatelink/gateway-endpoints.html) | No additional gateway endpoint charge and supported routing scope |
| S58 | [VPC pricing](https://aws.amazon.com/vpc/pricing/) | NAT processing/hourly cost, same-region S3 example and endpoint distinction |
| S59 | [AWS NLB transfer-cost optimization](https://aws.amazon.com/blogs/networking-and-content-delivery/optimizing-data-transfer-costs-when-using-aws-network-load-balancer/) | 2026 directed-edge sender/receiver charges and cross-zone hairpins; not universal LB pricing |
| S60 | [AWS zone affinity](https://aws.amazon.com/blogs/architecture/improving-performance-and-reducing-cost-using-availability-zone-affinity/) | Zone-local traffic pattern with multi-AZ fallback/capacity considerations |
| S61 | [AWS data-transfer architecture overview](https://aws.amazon.com/blogs/architecture/overview-of-data-transfer-costs-for-common-architectures/) | Network path classes and cost topology; historical rates need current verification |
| S62 | [GCS pricing](https://cloud.google.com/storage/pricing) | Storage operations and same-location transfer rules; region versus multi-region distinction |
| S63 | [GCP VPC pricing](https://cloud.google.com/vpc/pricing) | Inter-zone VM traffic and explicit GiB units; separate from GCS transfer exemptions |
| S64 | [Azure availability-zone overview](https://learn.microsoft.com/en-us/azure/reliability/availability-zones-overview) | Current same-region inter-zone transfer exemption and actual service failure-domain distinctions |
| S65 | [S3 FAQ](https://aws.amazon.com/s3/faqs/) | Same-region transfer, Express One Zone placement and upload/retrieval fee structure |
| S66 | [AWS KMS pricing](https://aws.amazon.com/kms/pricing/) | Key/request costs and example tariffs; application calls remain separately metered |
| S67 | [S3 Bucket Keys](https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucket-key.html) | SSE-KMS request reduction scope, not a reduction of hsearch application key-provider calls |
| S68 | [WarpStream distributed mmap](https://www.warpstream.com/blog/minimizing-s3-api-costs-with-distributed-mmap) | Multi-partition batching, metadata and cache economics; different authority model |
| S69 | [AWS request/retrieval cost analysis](https://aws.amazon.com/blogs/storage/analyzing-request-and-data-retrieval-charges-to-optimize-amazon-s3-cost/) | Small-object/request patterns, measurement and optimization |
| S70 | [AWS small-object compaction](https://aws.amazon.com/blogs/storage/optimizing-storage-costs-and-query-performance-by-compacting-small-objects/) | Compaction's storage/query tradeoffs; hsearch must include compute/request/rewrite costs |
| S71 | [pstore batching and visibility](https://github.com/huyz0/pstore/blob/6ddaf56dd0e34862561b63b173ac3de5cfb50fb4/docs/research/05-storage-engine/batching-and-visibility.md) | Producer/flush floors; cost extension rejects speculative-visibility or hidden metadata shortcuts |
| S72 | [pstore AZ topology](https://github.com/huyz0/pstore/blob/6ddaf56dd0e34862561b63b173ac3de5cfb50fb4/docs/research/04-cluster/az-topology.md) | Zonal cells over object storage; cache/DNS/universal-price assumptions evaluated critically |
| S73 | [turbopuffer tradeoffs](https://turbopuffer.com/docs/tradeoffs) | Object write latency/throughput, metadata-check and cold-cache design tradeoffs |
| S74 | [Fork fencing closure RFC](https://github.com/huyz0/OpenSearch/blob/ad254f6439dc4e93f5f00461b85a086e53a3d2da/serverless/rfc-fencing-closure.md) | Own-term establishment, stale-writer closure and conditional-create fences; fork design, not a hsearch proof |
| S75 | [Fork WalStore](https://github.com/huyz0/OpenSearch/blob/ad254f6439dc4e93f5f00461b85a086e53a3d2da/serverless/shell/src/main/java/org/opensearch/serverless/store/WalStore.java) | Inspected appendOne/establish/fenceOlderTerms implementation, serialized slots and failure poisoning; no fork test execution |
| S76 | [Fork WalGroupCommitter](https://github.com/huyz0/OpenSearch/blob/ad254f6439dc4e93f5f00461b85a086e53a3d2da/serverless/shell/src/main/java/org/opensearch/serverless/store/WalGroupCommitter.java) | Bounded serialized group commit and poisoned-lane behavior; implementation precedent |
| S77 | [SlateDB manifest RFC](https://slatedb.io/rfcs/0001-manifest/) | Manifest generations and writer fencing; historical RFC, read with later WAL/GC revisions |
| S78 | [SlateDB GC boundary RFC](https://slatedb.io/rfcs/0026-garbage-collector-boundary/) | Delayed stale creates after GC, retained boundary objects and sequence-aware reclamation; design precedent |
| S79 | [SlateDB pluggable WAL RFC](https://slatedb.io/rfcs/0030-pluggable-wal/) | Separate WAL/manifest fencing, serialized conditional creates and replay boundaries; RFC, not provider conformance evidence |
| S80 | [turbopuffer CMU transcript](https://turbopuffer.com/blog/video-andy-pavlo-cmu) | Object upload plus manifest CAS protocol and latency/metadata tradeoffs as described by its author; not hsearch's required protocol |
| S81 | [WarpStream write path](https://docs.warpstream.com/warpstream/overview/architecture/write-path) | Classic batched object uploads with metadata sequencing; separate service costs and authority |
| S82 | [WarpStream Lightning sequencing](https://www.warpstream.com/blog/the-art-of-being-lazy-log-lower-latency-and-higher-availability-with-delayed-sequencing) | Delayed sequencing and changed offset-order contract; not a free optimization for conditional document mutations |

## Evidence limits

The SlateDB `/docs/design/manifest/` URL failed retrieval during the initial pass. The immediate cost pass inspected the accessible manifest/WAL/GC RFCs S77-S79; no conclusion relies on the failed page. Seeded batching simulations and a bounded atomic-slot model were executed for M00.07, with [results and limits](../evidence/cost-research-results.md). No cloud account, hardware fleet, RustFS server, AI reviewer run, or engine build was exercised for this planning task. Repository-reported coverage, test counts, and benchmark timings remain reported history. M02, M03, and the qualification milestones replace those runtime evidence gaps with reproducible measurements.
