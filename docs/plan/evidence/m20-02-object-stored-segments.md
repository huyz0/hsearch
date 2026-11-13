# M20.02 evidence: engine over object-stored segments

## Upstream check (PR-53)

`lucene_store::Directory` at the pinned rev is already a pluggable trait (whole-file `open`, outputs through
`FsIndexOutput`), and `hsearch-engine` already accepts any `Directory`. No lucene-rust change was needed and no
engine behaviour is reimplemented locally: the new `CachedDirectory` only decides where bytes come from.

## What was built

- `RemoteFiles` (engine-owned seam, no store dependency) and `CachedDirectory<R>`: a manifest of remote file names,
  whole-file fetch into a bounded local cache (LRU eviction of remote-backed files only), delegation of every write,
  rename, lock and sync to a local `FsDirectory`. Nothing lists the store; opening costs one fetch per missing file.
- `Snapshot::open_committed(directory, schema)`: a read-only snapshot of the last native commit, with no writer, lock
  or retry state. This is how a search role serves committed segments.
- Finding that shapes M20.03: `Engine::open` over existing native files needs the in-process `Checkpoint`; native files
  alone cannot rebuild retry receipts. Recovery therefore replays the committed log into a fresh directory, and
  searchers use `open_committed`. M20.03 plans for that.

## Results (provisional: one container, 4 vCPU, plain-HTTP RustFS 1.0.4 on loopback)

- 40 documents in 4 engine batches, committed to a local directory, then uploaded file by file (81 native files).
- Counts and sorted hit ids for `All`, a keyword term and an integer range are identical over the local directory,
  over a remote-backed directory with a fresh cache, after `evict_all`, and after deleting the cache files behind the
  directory's back. Same equality against a real RustFS bucket (`tests/remote_rustfs.rs`).
- Opening costs one HEAD plus one GET per file (162 each for two cold opens) and no LIST. These are whole-file
  fetches; range-native reads are M04.02.
- A file larger than the cache bound, an unavailable store and a missing file are errors, not partial results; the
  directory recovers when the store does. The cache directory is emptied at creation, so a stale file is never read.
- Existing differential fixtures and the rest of the workspace suite still pass.
