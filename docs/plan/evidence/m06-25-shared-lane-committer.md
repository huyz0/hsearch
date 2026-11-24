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
  handle (ratio 3.3, one run, relative only), bounded by a 100x assertion (a guard, not a target).

- one host's bad body beside another's good record in one packed call: each gets its own outcome;
- a lane thread that dies (a planted panic) leaves `Unknown` to the caller and every later caller,
  never a refusal, and raises the shared flag for every handle.

Added after independent review: a lost reply is `Unknown` (a refusal would let the caller reuse the
sequence or delete a staged payload the log may name), the thread's death breaks the lane, and
packing stops at 8 MiB of bodies as well as 1,024 requests. Not covered: `commit_external` through
the lane thread (the held-job path) has no test of its own; the payload path is covered through the
direct committer.

Mutations seen red: the packing loop, the reply split, the lost-reply outcome (the flag set in `gone` is redundant with the thread guard, so removing it alone stays green), the poison flag (both conditions together;
either alone is enough for the fence test, so the other is defence in depth).
Existing store and node suites pass with the trait in place.

## Limits

- The head cover stays 0 on a shared lane: no reclaim is claimed (M06.26).
- A host's lane cover is one past its own last record, which is valid on a shared lane because the
  base covers all of that index's records.
- Membership changes, the durable incarnation and seal on open are M06.26.
