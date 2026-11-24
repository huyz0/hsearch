# M06.24 per-index base pointer

`provisional`: in-memory object store and local scratch engines on one Linux host.

## What exists

- **`BasePointer`** (`crates/format/src/pointer.rs`, envelope Kind 13, key `indexes/<index>/shards/<shard>/<incarnation>/base`):
  manifest root and its generation, the index's covered sequence, the lane (domain, incarnation,
  activation, number) the log above the base is on, the lane cover (the lowest lane ordinal that
  can hold a record of the index above `covered`), a sequence that starts at 1, and a commit token.
- **`validate_successor`**: sequence exactly one higher, same index, shard and incarnation, covered
  and manifest generation never fall, the lane cover never falls on the same lane, and the lane
  cover is never below the lane head's cover (a reclaim may already have passed it).
- **Store** (`crates/store/src/pointer.rs`): `BaseSource::{Head, Pointer}`, read (one HEAD and one
  GET) and write (successor rule, then create-if-absent or compare-and-swap on the version read).
  `Publisher::activate_pointer` verifies the root exactly as the head path does (boundary, digests,
  lengths, dependencies) and writes the pointer; `Publisher::read_pointer`.
- **Readers**: `Target.base` (recovery and rebuild; replay starts at the pointer's lane cover when
  it is on the lane scanned first), `ShardRef.base` (sweep and snapshot pins), `BaseTarget.base` and
  host `Config.base` (open and base publishing), `Compactor::compact_pointer`.
- A shard with no pointer and `BaseSource::Head` behaves as before; every existing test is unchanged
  apart from the new field.

## Evidence

`crates/format/tests/pointer.rs`: round trip, corruption, unknown version and kind, identity
mismatch, every successor refusal. `crates/store/tests/pointer.rs` (7): the pointer names the base
and starts the scan at its lane cover while the head names none; no pointer replays the whole log;
a stale read loses the swap and changes nothing; a lane cover below the head's is refused;
recovery from the pointer equals recovery from the head on a dedicated lane; a base published and
never named leaves the old one serving; a garbage pointer blocks recovery. Also `gc.rs` (compact
and sweep through the pointer, a wrong source refused), `snapshot.rs` (a pin holds the pointer's
root), `base.rs` (a serving writer publishes through the pointer and recovery opens it) and
`host.rs` (a host publishes bases on a schedule through the pointer; a restart serves every
acknowledged write and recovery starts above the second base).

Mutations seen red: the replay start (`lane_cover.max(head.cover)`), the lost-race mapping, the
pointer sequence, the source selection in `named`, the serving writer's pointer branch, and (format
crate) the successor rule's checks.

## Limits

- The lane cover a serving writer writes is one past its own last record, right only for a lane the
  index owns alone. A shared lane needs the pinned-set cover of the M06.21 decision (M06.26).
- Creation detection and the node's open still use the lane head; moving a node's indexes onto
  pointers is the M06.21 integration.
- A crash between the base objects and the pointer leaves orphans for the sweep, as it does for a
  head transition.
