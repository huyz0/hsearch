# hsearch-object

M03.01 defines provider-independent object and conditional metadata contracts.
The memory backend is a bounded, nonpersistent conformance double. No provider,
fsync, runtime authority, format, encryption or service is implemented.

`ObjectBackend` exposes immutable create, latest metadata and exact-version
ranges. `MetadataStore` adds atomic conditional replacement and resolution using
the full original mutation. `Precondition` cannot express unconditional overwrite.
Version tokens are opaque equality values; memory tokens are scoped to one
backend instance and key. Historic versions remain readable until that instance
is dropped or explicitly refuses capacity admission.

Each mutation identity binds key, payload, metadata and precondition. Successful
replays return the original receipt even after later updates. Changed content
under an accepted identity fails. Unknown outcome means the write may already
exist: resolve the original envelope with a live context. A missing resolution
is `ResolutionUnavailable`, not evidence that no write happened. The reference
backend never evicts accepted identities or old versions to make a retry look new.

The context charges input bytes and calls before work, and complete response
bytes before copying or accepting a mutation. These counters are conservative
admitted in-process contract bytes, not measured provider HTTP traffic. Response
reservation can remain charged after cancellation, conflict-free commit followed
by response loss, or a later refusal. A clone/boxed dispatch has finite overhead;
these byte budgets do not claim an allocator/RSS sandbox. Memory capacity charges
version key/body/metadata and retained mutation/receipt data separately; count caps
also bound map, token and allocation overhead. The memory backend retains at most
4096 keys, 16384 versions and identities, 64 MiB per retained-data category, and
32 pending faults. Caller-supplied lower caps are enforced before content copies.

All new admission observes cancellation and deadline, including after a mutex
wait. After commit, a known receipt remains truthful; faulted cancellation,
expiry or lost response returns unknown rather than claiming rollback. Contexts
have a maximum five-minute duration and can be shortened, never extended. Retry
classification borrows the unchanged request; it performs no retries or budget
reset. Scheduling and provider-specific capabilities remain adapter work.

The reusable public-trait history probe is `tests/support/mod.rs`. A later adapter
can include it in dev-only integration support and provide a fresh empty backend.
It never reads memory-private maps. `tests/conformance.rs` includes an intentionally
unconditional CAS adapter that the exact same predicate must refuse. Dedicated
fault, concurrent-CAS, identity, range and budget controls use the explicit memory
fault seam. Real provider conformance remains required by later tasks.
