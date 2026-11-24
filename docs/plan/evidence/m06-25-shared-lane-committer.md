# M06.25 shared lane committer

`provisional`: in-memory object store and local scratch engines on one Linux host.

## What exists

- **`Log`** (`crates/store/src/log.rs`): `commit`, `commit_external`, `is_poisoned`, implemented by
  the direct `Committer`. `Serving<L: Log>` and the host's core are generic over it;
  `Host::open_log(LogArgs, Config)` opens a host over any log and `Host::open` is the direct one.
- **`SharedLane`** (`crates/store/src/shared.rs`): one thread owns the lane's committer. Hosts commit
  through cloneable `LaneHandle`s; the thread packs the jobs waiting (up to 1,024 requests, held for
  an optional wait) into one committer call, so records of different indexes share a lane slot, and
  returns each job its own outcomes. A shared poison flag is raised when an append is fenced or
  poisoned, so every handle's `is_poisoned` turns true.

## Evidence

`crates/store/tests/shared.rs`:
- two hosts (two indexes, bases on a schedule through their pointers) on one lane through one lane
  thread, restarted three times over a fresh lane thread, serve exactly their acknowledged writes
  and nothing of the other index;
- two handles committing at once land in the same slot (equal ordinals) with a 500 ms hold;
- a second writer taking the lane fences it: the host that wrote gets an error, the other host's
  next write is refused, and the shared flag is set;
- latency: 200 sequential single-record commits take 5.6 ms direct and 18.2 ms through the shared
  handle (ratio 3.3, one run, relative only), bounded by a 10x assertion.

Mutations seen red: the packing loop, the reply split, the poison flag (both conditions together;
either alone is enough for the fence test, so the other is defence in depth).
Existing store and node suites pass with the trait in place.

## Limits

- The head cover stays 0 on a shared lane: no reclaim is claimed (M06.26).
- A host's lane cover is one past its own last record, which is valid on a shared lane because the
  base covers all of that index's records.
- Membership changes, the durable incarnation and seal on open are M06.26.
