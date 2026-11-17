# M06.02 publish segments and archive coordinates

`provisional`: unit and integration level against the in-memory store with an injected-write-failure wrapper, one Linux host. No RustFS here (no Docker daemon), so nothing below is claimed for a real object store.

## What exists

- **`hsearch_engine::upload_files` / `upload_local`** (`crates/engine/src/publish.rs`): every named native file is verified with `verify_files` (footer CRC) **before the first byte is uploaded**, so one damaged file uploads nothing; each file then streams through `ObjectUploadSink` (create-only, at most one part buffered) while the exact bytes sent are hashed with SHA-256. Returns name, length and digest per file.
- **`hsearch_store::publish::Publisher`** (new crate `hsearch-store`, `crates/store/src/publish.rs`):
  - `publish(files, names, spec, dependencies)`: refuses a base whose `covered` is above the acknowledged prefix (`Speculative`) or below the previous base's (`Regression`); checks the mapping and key dependency (`Dependencies` trait) before and again after the upload (`Stale`); uploads the verified files under object ids derived from the attempt token; HEADs each and compares the length; writes the manifest pages (bounded 2,048 entries each, create-only), then the root last. The head is not touched.
  - `verify_root(...)`: reads the root, checks every page against its digest and entry count, and HEADs every file at its recorded length. One GET for the root, one per page, one HEAD per file.
  - `activate(...)`: checks the spec matches the published root (index, covered prefix, covered not above acknowledged), runs `verify_root` first (which now also re-checks the mapping and key dependency), then one `Checkpoint` compare-and-swap of the structural head to name the manifest and cover. A head that moved is `Lost` and nothing changes; a lost reply that cannot be resolved is `Unknown` (re-read the head), never `Lost`.
- **Coordinates.** `covered` comes from `Cut`, which only the replay barrier (`Replay::cut`) can make, so it is a contiguous applied prefix; the caller also passes the acknowledged prefix and the publisher refuses to pass it. A base never claims history that is not committed.
- **Orphans.** Every object is created once. A retry uses a new attempt token and so new object ids; objects of a failed attempt are unreferenced orphans for GC (M06.04), never reused.

## Evidence

`crates/store/tests/publish.rs`, 11 tests, real engine commits on disk:

- A published base: every file object equals its local file byte for byte and its recorded digest, `verify_root` passes, the head names nothing until `activate`, then names the manifest and cover.
- Speculative and regressing bases are refused with zero store writes; forward bases take the next generation.
- A flipped byte in the largest native file (it still opens) uploads nothing (`Unverified`, zero writes).
- A stale dependency stops a base before any write, and one that goes stale after the upload leaves only the files (no page, no root).
- **A crash at every write boundary** (create or upload step failing from write 0 to the last, ~20 boundaries): the head is exactly as before, a root that exists lists only objects that exist (it is written last), and a retry under a new attempt on a healthy store publishes and activates a base that verifies.
- `verify_root` refuses a tampered page, a deleted file object, and `activate` refuses to name an unverified root; a head read before another transition won is `Lost` and the winner stands.
- A base of 2,104 files spans two pages and verifies.
- **A lost reply at every write** (the write commits, the reply is `UnknownOutcome`): pages, root and the head transition resolve by identity and the base verifies; a file's create is not resolved by the upload sink, so the publish reports `Upload` and publishing the same attempt again replays the one-part files that landed (tested for all of them). An unresolvable lost reply on the head transition is `Unknown`, and re-reading shows it landed.
- Activation re-checks dependencies (`Stale`) and refuses a spec that does not match the published root (`Mismatch`); the same attempt published twice yields the same manifest, a new attempt a new one; no files is `NoFiles`.

Mutations each seen red: `verify_files` removed from the upload, the speculative check removed, `activate` not verifying the root, pages written after the root, `UnknownOutcome` not resolved in `create`, an unknown head outcome reported as `Lost`, and the spec/root match removed. The tests were written after the code, then made to fail by these mutations.

## Limits

- **In-memory store only.** The crash test fails writes before acceptance and a second test loses the reply after acceptance; the in-memory store's identity resolution is the reference, not a real provider's. RustFS and real S3 are not run.
- **Multipart files** (over one part, 5 MiB) have no replay: a failed or unknown multipart file needs a new attempt, and its earlier objects are orphans. Only one-part files were exercised.
- **Integrity of the uploaded bytes** is the SHA-256 and length of exactly the bytes sent plus a HEAD length check; the uploaded objects are not read back and re-verified, because that would cost a GET per file. `verify_root` also checks lengths, not content digests of files. A flipped byte inside a stored file is therefore found by the engine on open or by `verify_files` on a fetched copy, not by `verify_root`.
- The lane `cover` passed to `activate` is the caller's: this task does not map index sequences to lane ordinals for shared lanes (M06.03 recovery owns that).
- `Dependencies` is a trait with an accept-all implementation; the live mapping and key-version view is wired in M06.05. The key envelope is not yet fetched.
- Files of one base are uploaded one at a time (no parallel upload); no throughput measured.
- `Error::TooManyFiles` (more than 2,048 x 4,096 files) is not exercised.
- Dev builds only; coverage was not measured locally (no `cargo llvm-cov` here), CI's coverage job decides.
