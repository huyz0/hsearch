# M06.12 recover a writer from a base and the committed tail

`provisional`: in-memory object store, local scratch directories on one Linux host. No RustFS here, so nothing below is claimed for a real object store.

## What exists

- **Writing the list with the base.** The tombstone list (M06.11) now ends in the native CRC-32 footer, so it is verified like any native file. `LocalFiles::write_tombstones(&checkpoint)` writes it into the scratch directory as `hsearch.tombstones`; publishing it with the native file names makes it a manifest entry with a length and SHA-256 digest like the rest. The manifest format is unchanged.
- **Reading a base.** `Publisher::entries` returns the files a root names (each page checked against the root). `Publisher::fetch` downloads one entry into a directory: the length is checked against the object first, the file is written in ranges of at most 8 MiB (one range in memory), the SHA-256 is compared with the manifest's, and a file that fails either check is removed. A name that could leave the directory is refused and an existing file is never overwritten.
- **`recover_writer(publisher, target, dependencies, schema, scratch, run)`** (`crates/store/src/rebuild.rs`). Reads the head. With no base it replays the whole log into an empty engine. With a base it verifies the root, fetches every file and the tombstone list into `scratch` (emptied first), opens the engine with `Engine::open_recovered` on them, replays the committed tail onto it through `rebuild`, and runs the caller's closure with the engine and what was replayed. If the head names a different base after the replay it is `BaseMoved`. A base with no tombstone list is `MissingTombstones`. `rebuild` called on an empty engine still refuses a base (`BaseNotOpened`).

## Evidence

`crates/store/tests/rebase.rs`, 5 tests. The whole history is committed to a lane by the real committer; a writer engine applies the first three batches, commits, and publishes its files and tombstone list as a base activated at that point; the last two batches are the tail.

- A fresh recovery matches the independent model (`tests/support/history.rs`, which never uses the engine) on presence, version and stored value of every identity, on the receipts of each tail batch (outcome, version, id), and on refusals: a put at version 2 over `b`, deleted at version 3 **inside the base**, is `Conflict` (only the tombstone list carries that), and one at version 9 succeeds. Only the tail was replayed (`from_seq` 3, `replayed` 2).
- The writer's own scratch is deleted and the recovery scratch is left holding the first recovery's files: a second recovery empties it and gives the same state.
- A base published without a tombstone list is `MissingTombstones`; a base file object removed from the store is `NotReady(Base)`.
- A scratch directory that is non-empty and was not marked by an earlier recovery is refused (`Scratch`) with its contents untouched, on both the base path and the no-base path; an empty or previously marked one is reused. The mark is a sibling file `<dir>.hsearch-scratch`.
- `fetch`: an entry with a wrong digest is `Broken("file digest")` and leaves no file; a wrong length is `Broken("file length")`; `../escape` is `Broken("file name")`; fetching over an existing file is `Broken("scratch file")`.

Mutations seen red: the digest check removed, the length check removed, the missing-list check removed, the tombstones not applied, the scratch not emptied. **Not covered:** `BaseMoved` (the head moving between opening the base and the end of the replay) has no test, because nothing here can move the head mid-replay; the comparison is plain and the failure is a retry.

## Limits

- **No production path writes the tombstone list yet.** `write_tombstones` exists and the test publisher uses it, but nothing that publishes a base in production does: until M06.05 wires the writer's publish step to write it, `recover_writer` refuses every real base with `MissingTombstones`. Writing it at base publish is a M06.05 deliverable.
- `rebuild` still refuses a head that names a base; `recover_writer` is the base path.
- Not tested: the head moving during the replay (`BaseMoved`) and the other head-move outcomes; files left in scratch after a later failure (the next recovery empties them); two entry names differing only in case on a case-insensitive filesystem.

- Provisional: in-memory store, no RustFS, no timing or request counts (M06.08).
- The base is **not pinned** while it is fetched. A collection sweep or a compaction that replaces it during the fetch makes the fetch fail (`NotReady(Base)` when an object is gone), never serve a mixed base; registering a pin (`Pins::register`, `Pin::usable` before each read) and the collection boundary check belong with the writer wiring in M06.05.
- Files are fetched one after another through one range at a time; no parallelism and no disk budget yet.
- Request history is not restored (M06.11): a retry of a request from before the base is refused as stale rather than recognised.
- Spill past the replay bounds, external payloads and sealed successors are M06.13.
- Tests were written after the code, then made to fail by the mutations (disclosed departure from test-first).
