# M20.03 evidence: write path v0

Provisional: real RustFS 1.0.4 (digest-pinned) on a shared cloud container, debug build.

Design (crate `hsearch-index`): one writer thread per index owns a scratch engine. A group of queued bulks becomes one log slot at `{index}/log/{seq}`, created with a conditional create (derived key, v0 marker, disposable format). The slot is durable before it is applied, so a definite store refusal leaves the engine untouched. `Wait::Durable` acknowledges at that point; `Wait::Searchable` acknowledges after apply and refresh. Publish commits the engine, uploads new segment files under `{index}/seg/{epoch}/`, then swaps `{index}/root` by compare-exchange. Open replays slots `0,1,2,...` until the first not-found into a fresh engine, then bumps the root epoch, which fences any earlier writer.

Fault matrix (`crates/index/tests/rustfs.rs`, a child process aborted at each point, then a fresh process rebuilds with all local files deleted), run three times, all green:

| Abort point | Result after recovery |
|---|---|
| before the slot write | acknowledged bulks all present; the in-flight bulk absent |
| after the slot write, before the ack | acknowledged bulks present; the durable but unacknowledged bulk also present (replayed), never partial |
| after the ack, before apply | the acknowledged bulk is present after replay |
| mid-publish (files uploaded, root not swapped) | all acknowledged bulks present; the old root still serves; orphan files harmless; a later publish succeeds |

Each case also checks: every bulk is wholly present or wholly absent, no unacknowledged bulk is visible except the one whose slot was durable, and the recovered writer accepts a new bulk, publishes and serves it through a search-only reader.

Requests (real store, `S3Store::counts`): one bulk of 100 documents is exactly 1 PUT, 0 GET, 0 HEAD; a refresh makes 0 requests; a publish is one PUT per new file plus one for the root; open is one GET and one HEAD per slot plus the root and one HEAD past the end; LIST is 0 on every path.

In-memory fault tests cover refusal before acceptance, lost response after commit and expiry after commit (resolved by identity, acknowledged once), fencing of a stale writer, corrupt slot refused rather than read as the end of the log, capacity refused before any slot is written, and a full engine committing and merging before retry.

Found and fixed on the way: an engine merge only compacts what a commit has flushed, so the writer commits before merging when the engine reports it is full.

Out of scope, listed per the task: fencing across concurrent owners beyond refusing a stale root, multi-node routing, authority barriers, the coalesced refresh and visibility target (M06.07), writer recovery from segments (the engine reopens only from an in-process checkpoint, so a restarted writer replays the whole log), multipart upload of files above 16 MiB (M03.10), sharing a log between indexes (M03.09).
