# M06.26 lane membership, seal on open and lane cover

`provisional`: in-memory object store and local scratch engines on one Linux host.

## What exists

- **`LaneCover` / `SharedCover`** (`crates/store/src/cover.rs`): the pinned set of a shared lane. A
  host is pinned from its open (from the position recovery started, `Recovered.scan_from`) and from
  every record it commits; a published base covering what it wrote unpins it unless it wrote since
  the cut. The cover allowed is the lowest pinned slot, or one past the highest slot reported.
  `overdue()` names pins that have held a slot span bound, `release_promoted` releases a promoted
  index only when its new-lane base covers its seal, `forget` drops a deleted one.
- **`Publisher::raise_cover` / `advance_cover`**: a head `Checkpoint` transition whose root is a
  witness derived from the lane and the cover (no object behind it), refused when the head's writer
  epoch is not the one the pins were computed under. One head read, and one CAS when it moves.
- **Host**: `Config.cover` pins the lane at open, publishes a base when its pin has held the span
  bound (no schedule needed) and when it stops with unpublished writes, and raises the head's cover
  after each base is named. A base's lane cover is `max(own last slot + 1, head cover)`: a cover that
  passed an index whose earlier base covered everything it wrote is not a reason to refuse its next
  base (found by the host test; without the max the second publish after the cover moved failed).
- **Seal on open** (`crates/store/src/tail.rs`): reads the slots above the head's cover and returns
  the indexes with a record their pointer's base does not cover (first such slot, covered sequence),
  as members or promoted ones; a deleted index is left out. Bounded by `max_records`.
- **Incarnation**: `served::index_id_in(name, incarnation)`; a reused name has another identity, so
  recovery by it replays none of the old records. The catalog binding that hands the incarnation
  out is the node's part (M06.27).

## Evidence

`crates/store/tests/cover.rs` (10): the cover never passes a pinned record, a stale writer epoch and
a cover not above the head's are refused; a write after the cut keeps the pin from the cut; a pin
that held its bound is overdue; a promoted pin is released only by a covering new-lane base; an idle
index restarts after the cover and reclaim passed its pointer (the replay starts at the higher of
the two); the tail scan finds only uncovered indexes (a base covering part of an index's records
moves its first slot), reads no slot once the cover is at the end, leaves a deleted index out and
keeps a promoted one, and refuses a tail over its bound; a reused name replays nothing; one commit is
one lane write at 1, 100 and 1,000 members. `crates/store/tests/shared.rs` (+2): two hosts pin the
cover until each publishes, the cover then moves to the end, a write re-pins from its record,
stopping publishes only what is unpublished, and a restart serves every acknowledged write; a host
publishes by its pin bound with no schedule.

Mutations seen red: the replay start, the allowed minimum, the unpin rule, the cut rule, the epoch
check, the promoted release, the deleted/promoted split, the tail's first uncovered record, the
lane cover max, the open pin, the cover advance after a base, the stop guard, the span trigger.

## Limits

- Not wired into the node yet: creating an index joining the lane, delete sealing and leaving it with
  a route tombstone, the catalog's incarnation binding, and the lane thread reclaiming slots below
  the raised cover are M06.27. `Committer` exposes the writer read-only today.
- The span bound is in lane slots, not bytes (a slot's size is bounded by the batch policy).
- A node asks `SharedCover::overdue` after a commit to reach an index that stopped writing; the
  call site is the node's.
- Requests per restart were not counted: the tests assert that a restart with every index published
  reads no slot and finds no index; the per-slot and per-pointer counts follow from the code (two
  requests per slot, one descriptor read, one pointer read per index in the tail).
