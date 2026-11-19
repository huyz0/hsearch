# M06.11 open an engine on a base's files and tombstones

`provisional`: local files on one Linux host; no RustFS, no object store in this task.

## Why this task exists

M06.10's rebuild refuses a head that names a base, and opening the engine on the base's files was meant to be a small step. It is not, because **a reopened Lucene index does not carry everything the writer must remember**. `Checkpoint` is documented as in-process only; the engine's per-identity state is `current: id -> (version, digest, docs)`, and a delete leaves no document, only a tombstone version in that map. A writer reopened from native files alone would accept a put at an older version over a deleted identity. The digests are also keyed per process, so they cannot be written down and read back; they have to be recomputed from the stored values.

## What exists

- `Engine::open_recovered(directory, schema, tombstones, mode)`: opens on a committed base's files in a process that holds no checkpoint. Live identities, versions and digests are rebuilt from the index (`reader::rebuild`, the inverse of the checkpoint verification; the two now share one `scan` pass, so a reopened checkpoint and a recovered base are read by the same code). Deleted identities take their versions from the list. An empty directory is refused.
- `Checkpoint::tombstones()`: the list for the commit that checkpoint names (magic, the 32-byte native commit identity it was taken at, count, then identity and version in strictly increasing identity order). Decoding refuses anything that is not whole: a prefix, trailing bytes, a bad magic, a count the bytes cannot hold, an empty or over-long identity, a version that is not positive, a repeated or out-of-order identity. When the engine opens it also refuses a list taken at a different native commit than the files (otherwise another commit's list would silently forget a delete) and a tombstone for an identity the index holds live.

## Evidence

`crates/engine/tests/recovered.rs`, 7 tests:

- A history of puts, an update, a delete and a delete of a never-seen identity is committed; only its native files and tombstone list are placed in a fresh directory and opened in a fresh engine. A put at an equal or lower version over a live document, a put at or under a tombstone's version (including the never-seen identity's) are `Conflict`; a higher version succeeds as `Updated` or `Created`; an unseen identity is new; the engine commits again.
- The same continuation applied to the engine that wrote the history and to the recovered one gives the same outcomes in the same order.
- Every strict prefix of the list, a trailing byte, a bad magic, an oversized count, no files, a tombstone for a live identity, a tombstone version of zero or below, and an out-of-order or repeated list are each `Checkpoint`. The first commit's list against the second commit's files (a delete would be forgotten) is `Checkpoint`; the second's list opens.
- A history over keyword, text, integer, boolean, date and nested values is recovered, extended, committed, and the recovered engine's own checkpoint reopens over the same directory: the reopen verifies every stored value against the digests the recovery rebuilt from the index, so the rebuilt digests are the ones apply computes for those kinds (join and vector are not exercised on this path; they share the same scan).

Mutations seen red: tombstones not restored, the live-and-dead check removed, the tombstone filter inverted, the positive-version check removed, the commit binding removed, the order check relaxed. **Two survive:** dropping the version comparison from the checkpoint scan (no existing test reopens a checkpoint over files whose version differs but whose native commit identity matches, a gap that predates this task), and the per-root native document count (`docs`) used when a merge recomputes physical documents; neither is observable through the engine's API in these tests.

## Limits

- The list format was revised in M06.12 (`HST3`: the same list followed by the native CRC-32 footer, so a publisher verifies it with the native files); the tests here use the current format.

- **Request history is not restored.** A retry of a request from before the base is not recognised; it is refused as a stale version instead (and a first-time request is unaffected). Recognition resumes for requests since the base, including the tail replayed onto it.
- The tombstone list grows with deletes and has no retention policy yet; `MAX_TOMBSTONES` bounds one list and a larger one is refused at encode, so a base that deletes more than that cannot be published until compaction of tombstones exists.
- **Counters restart.** The recovered state starts with zero operations and history bytes, where a checkpoint reopen carries them, so a writer gets a fresh `limits.operations` budget after a recovery.
- `Checkpoint::tombstones()` is an accessor: nothing here writes the list at commit, names it in a manifest or fetches files from the object store; that is M06.12.
- Tests were written after the code, then made to fail by the mutations (disclosed departure from test-first).
