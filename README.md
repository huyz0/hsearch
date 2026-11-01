# hsearch

A planned horizontally scalable Rust search store built around [huyz0/lucene-rust](https://github.com/huyz0/lucene-rust), with RustFS first and S3, GCS and Azure Blob as eventual storage providers.

Start with the [research and delivery plan](docs/plan/README.md). It covers architecture, module contracts, cross-cluster replication/copy, BYOK encryption, AI development/operations, conditional CI and Linux x86_64/aarch64 qualification.

The current deliverable is research and planning. Runtime implementation begins with M01 in the [roadmap](docs/plan/roadmap.md).
