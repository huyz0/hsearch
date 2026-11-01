# hsearch

A planned horizontally scalable Rust search store built around [huyz0/lucene-rust](https://github.com/huyz0/lucene-rust), with RustFS first and S3, GCS and Azure Blob as eventual storage providers.

Start with the [research and delivery plan](docs/plan/README.md). It covers architecture, module contracts, cross-cluster replication/copy, BYOK encryption, AI development/operations, conditional CI and Linux x86_64/aarch64 qualification.

The research and planning package and M01.01 [Rust workspace bootstrap](docs/plan/development.md) are integrated. M01.02 [capped local Linux sandbox](docs/plan/sandbox.md) is integrated. M01.06 [impact-selected CI](docs/plan/impact-ci.md) is under review. No search service exists yet. The [roadmap](docs/plan/roadmap.md) tracks the remaining runtime and qualification work.
