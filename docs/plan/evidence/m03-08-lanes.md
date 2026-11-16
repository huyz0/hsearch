# M03.08 evidence (increment 1): fenced commit lanes, shared lanes, promotion, head-CAS comparison

`provisional`: RustFS 1.0.4 (digest pinned in `scripts/rustfs.sh`) in a Docker container on a shared 4-CPU cloud VM, loopback, release build, one seed. No MinIO, no real S3, no network, no KMS. Numbers are relative. Reproduce: `eval "$(scripts/rustfs.sh up)"; cargo run --release -p hsearch-lane --example lane_profile -- all 1.0`. Counts are S3 adapter wire attempts (`S3Store::counts`); no LIST appears on any path.

## What is built (`crates/lane`, new crate `hsearch-lane`)

- **Fenced ordered-slot log** (`log.rs`): one conditional create per batch at the next position; one append in flight (`&mut self`); a conflict (`Fenced`) or an unresolvable lost reply (`Unknown`) poisons the writer; a certain refusal does not. `open` replays from the caller's cover to the first absent position and creates a retained fence there (PR 83's rule, now on the lane format). Fences are never deleted; `reclaim_below(cover)` removes data slots only. `reclaim_below` first raises a durable, monotonic cover marker (`COVER`, a 32-byte record kept here until the structural head of M03.05 takes it over); `open` reads it after replaying and refuses (`BadCover`) if the first absent position is below it, so a replay start inside a reclaimed run of any length cannot fork the log (M03.04's seal-above-the-cover rule). Reclaim is refused while a promotion started in this lane is unfinished below the cover, because a reopen could not know of the seal.
- **Shared lanes** (`members.rs`, `place.rs`): descriptor written only on membership change; identical policy tuple, ceiling 1,000 (format ceiling 4,096), BYOK alone (format-enforced); several joiners in one CAS; a placer finds the open group by doubling and bisection over derived keys (no LIST) and joins with one CAS.
- **Batcher** (`batch.rs`): sans-IO, bounded by 4,096 records and a 4 MiB byte target (a larger record is its own batch), wait from the first record; an idle batcher has no deadline.
- **Hot rule and promotion** (`hot.rs`, `promote.rs`): a quarter of a window's bytes on a lane with at least 1,000 records; promotion = seal record in the source log, create target lane, CAS the member out; resumable at every step. New format op `Seal` (record op 3, no payload).
- **Payload + head CAS** (`headcas.rs`): the alternative, with the same accounting. **Accounting** (`counting.rs`).

## Tests

33 tests in the lane crate (28 on the memory store with injected lost replies, refusals and unresolvable resolution; 4 threads racing to open and append over 20 rounds with the property "no acknowledged slot lands behind a later opener's fence, every ack is in the log"; crash after each prefix of a promotion; reclaim keeping fences and a late writer still refused; placement; batcher; hot rule; head CAS fencing) and 5 against RustFS (all run in CI). Four mutations were seen red by hand during development (no fence written, conflict not poisoning, seals ignored on replay, unknown outcome not poisoning); they are not kept as a mutation suite.

## Requests per write and latency (RustFS)

Append, one writer, 1 KiB records, 300 appends:

| protocol | records per batch | p50 ms | p99 ms | PUT per ack | GET / HEAD |
|---|---|---|---|---|---|
| fenced slot | 1 / 16 / 256 | 5.9 / 6.9 / 8.4 | 8.3 / 9.9 / 13.7 | 1.00 | 0 / 0 |
| payload + head CAS | 1 / 16 / 256 | 13.2 / 13.4 / 15.9 | 18.9 / 22.2 / 33.4 | 2.00 | 0 / 0 |

Fenced slot costs half the PUTs and about 45-55% of the p50 latency (41-44% at p99) of the head-CAS design in its **best case** (one writer, no contention, version token carried from the last CAS). The head-CAS design's contended cost (about 1.29 PUT, 1.38 HEAD, 0.29 LIST per commit, M02.10) is not re-measured here and only makes it worse.

Small-index sweep, M03.09's workloads and parameters at 1M active indexes (option A run at scale 0.05 because it needs a lane per written index; write storm capped at 0.1; per-lane arrival rates are unchanged by scale). Commit PUT per write is data slots only; the all-in column adds lane creation and open (once per lane in the window, not per write):

| workload | option | lanes | writes | PUT / write (commit) | records / slot | all-in PUT / GET / HEAD per write | hottest index share |
|---|---|---|---|---|---|---|---|
| steady_small | A lane per index | 1,059 | 3,000 | 0.968 | 1.0 | 1.67 / 0.35 / 1.06 | none busy |
| steady_small | B default-key shared | 1,000 | 56,999 | **0.199** | 5.0 | 0.23 / 0.02 / 0.05 | 0.003 (10 busy lanes) |
| steady_small | B BYOK own lane | 1,077 | 3,001 | 0.970 | 1.0 | 1.69 / 0.36 / 1.08 | none busy |
| write_storm | A | 22,566 | 30,000 | 0.995 | 1.0 | 2.50 / 0.75 / 2.26 | none busy |
| write_storm | B default-key | 100 | 57,056 | 0.175 | 5.7 | 0.18 / 0.00 / 0.01 | none busy |
| write_storm | B BYOK | 2,251 | 2,944 | 0.996 | 1.0 | 2.53 / 0.76 / 2.29 | none busy |
| one_hot_index | B default-key | 1,000 | 58,509 | 0.395 | 2.5 | 0.43 / 0.02 / 0.05 | **0.999** (1 candidate) |
| one_hot_index | B BYOK | 1,469 | 1,491 | 0.999 | 1.0 | 2.97 / 0.99 / 2.96 | none busy |

Readings:
- **Against the M03.09 model.** Default-key shared lanes: 0.199 PUT per write measured against the model's 0.247; the model's figure is blended over default-key and BYOK writes, and the blend here is (11,361 + 2,912) / 60,000 = **0.238**, 4% under the model. A: 0.968 against 0.999. Model confirmed within a few percent; the KMS term is not measured.
- **BYOK costs A's price**: 0.97-0.999 PUT per write and, counting the lane's creation and open once, 1.7-3.0 PUT per write on these sparse loads. That is the tension M03.09 recorded, now measured; option C's trigger is unchanged.
- **Hot index**: a lane with one index at half of all writes shows 0.999 of its bytes and exactly one promotion candidate; the skewed load peaks at 0.3% on 10 busy lanes and promotes nothing, as modelled. The floor matters: only these lanes cross 1,000 records in 30 s.
- **Promotion** on RustFS: 18.6 ms, 3 PUT / 0 GET / 0 HEAD / 0 LIST (seal slot, target descriptor, source descriptor CAS); a later write for that index to the source lane is refused (`Sealed`).
- **Lane open** costs 1 PUT (fence), 1 GET, 3 HEAD (descriptor HEAD, end-of-log HEAD, hole guard HEAD) plus 2 requests per replayed slot.

Re-establish time after a poison (median of 5 opens, 16 records per slot; counts are for the fifth open, which also replays the four fences earlier opens left):

| slots above the cover | open ms |
|---|---|
| 0 | 13.3 |
| 10 | 37.8 |
| 100 | 238.3 |
| 1,000 | 1,975.8 |

Replay costs about 2 ms per slot (one HEAD plus one GET: the version-pinned read API needs the HEAD first), so re-establish time is set by how far the cover lags, not by the lane's size. A poisoned writer that still holds its applied state reopens at its last applied slot: 13 ms. This is the blast-radius duration for a lane's 1,000 members.

## Downsides and trade-offs

- A shared lane stalls all its members while poisoned or being reopened (up to the figures above); ceiling and promotion bound it, not remove it.
- Replay reads each slot with a HEAD and a GET (2 requests) because reads are version-pinned; a successor that cannot be told its cover pays for every slot above it. Advancing the cover is M06's checkpoint.
- Retained fences accumulate one per open and are replayed by every later open above the cover; they are never reclaimed.
- Descriptor CAS rewrites up to 16 KiB per join; `join_members` joins many in one CAS, a placer joins one at a time.
- `open` reads the cover marker once (one HEAD when none exists, plus one GET when it does) for safety; `reclaim_below` costs one HEAD and one 93-byte ranged read per position plus one delete per data slot, and one marker PUT.
- Replay is serial (HEAD then GET per slot). The keys are derived, so a bounded read-ahead window is possible and would cut re-establish time; not done here.
- Placement of an index that already lives in an earlier group would add it to a second group: the catalog (M03.05) must remember the answer, and a crash-retry must consult it first.

## Increment 2: exhaustive fault histories and the head-driven open

`provisional`: same RustFS 1.0.4 container and caveats as above; the enumerations run on the in-memory store, the open costs on RustFS.

**Exhaustive histories** (`crates/lane/tests/histories.rs`). A scripted store wraps the memory store and applies a fault to the nth mutating request: refused, reply lost (resolvable by identity), reply lost (unresolvable), another writer's open lands first, another writer's batch lands first. Steps are open or append by either of two writers, crash of either, and reclaim. Every sequence is run and then checked: acknowledged batches are in the log exactly once and in order; no refused or unattempted batch is in the log; an unknown batch appears at most once; no acknowledgement after another writer's completed open; a dead (poisoned or fenced) writer sends no further request; consecutive acknowledgements have consecutive ordinals; a recovery open replays exactly cover..end and fences the end; an open below a reclaimed cover is refused. Head mode adds: the head's cover is the durable cover, inventory fences ascend, the epoch equals the inventory length, every successful open's fence is in the inventory, the tail is not above the log, the head generation is bounded.

| mode | length | faults | runs |
|---|---|---|---|
| marker | 5 | 1 among first 5 requests | 410,571 |
| marker | 4 | 2 among first 4 | 436,982 |
| head | 5 | 1 among first 6 | 521,017 |
| head | 4 | 2 among first 5 | 662,676 |

These four run on every push (about 2.5 minutes in debug together). A length-6 marker sweep (about 2.2 million runs, about 5 minutes) was run once for this evidence; the `#[ignore]`d long test also holds a length-6/7 head sweep that has not been run. The head sweeps do not enumerate the `BadCover` re-read (a checkpoint landing inside an open); that rule is pinned by one targeted test (`headopen`), and removing it fails that test and both marker sweeps but not the head sweeps. Retained fences are checked indirectly (reclaim deleting them fails all four sweeps). The generation bound allows two head transitions per open, so a doubled establish would not be caught. The bound is the bound: nothing is claimed about longer histories, more writers or more faults.

**Mutations seen red** (each by a `sed` on `crates/lane/src/log.rs`, run against the histories and headopen tests). Marker path, first round: conflict not poisoning, unknown not poisoning, reclaim deleting fences, an occupied open treated as sealed all survived the first sweep; the dead-writer-sends-no-request invariant, the longer sweep and the preempt faults killed them. Head path: establish skipped; the cover read from the marker instead of the head; the cover re-read after replay removed (killed by the new headopen test, not the sweeps); a later fence accepted in establish; the equal-fence lost-reply check removed; the activation check removed; the below-cover check in establish removed. All killed.

**Head-driven open** (`LaneWriter::open_with_head`). It reads the head, replays from the head's cover, probes the first absent position, re-reads the head's cover (a checkpoint that landed during replay and passed the end found is `BadCover` before any fence is written), creates the retained fence, then records the epoch with one head CAS (`Change::Establish`), retried on contention, and recognised as done if a lost reply had landed. A lane is opened by the head or by the marker, never both, and the writer remembers which (the other mode's reclaim is refused): `reclaim_to_checkpoint` removes slots below the head's cover and does not touch the marker.

| open (8 batches in the log, RustFS) | PUT | GET | HEAD | LIST | wall |
|---|---|---|---|---|---|
| marker open (replays 9 slots) | 1 | 10 | 12 | 0 | not timed, so the wall figure is not a comparison |
| head open (replays 10 slots, one more fence from the marker open) | 2 | 14 | 15 | 0 | 65-71 ms, 3 runs |

The head open costs one extra PUT (the establish CAS) and about two extra GET/HEAD pairs (the head read, the re-read after replay, the establish read); the rest is the one extra slot replayed. No LIST. Appends still write the head never.

**Downsides.** The epoch inventory is append-only with at most 1024 entries, so one lane can be opened by head about 1,023 times per activation before `Capacity`; the escape is a new activation, which belongs to M03.05's remainder. A head open that loses to a later opener is `Fenced`, and one whose cover was moved by a checkpoint during replay is `BadCover` after the visitor may already have seen slots, so a caller must treat a visitor's output as provisional until the open returns. The open costs a PUT and a few reads more than the marker open for the same lane.

## Not done here (stated, not claimed)

Catalog and activation routing, so a promotion has no routing publication and the promoted index has no writer until M03.05 provides one; engine apply and checkpoint publication that move the cover (M06); inline wrapped-key format and AEAD (M05); slot payload objects above the inline bound are the caller's; no multi-node soak, no MinIO/S3. The two-phase seal, the hole guard and the poison rules follow M03.04's model but this is not a model-checked implementation of it: the runtime histories above are tests and a race stress; the bounded enumeration in increment 2 is exhaustive only inside its stated bounds. Coverage is measured by CI (`cargo llvm-cov` is not installed in the dev sandbox).
