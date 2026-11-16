# M05.05: rotation and revocation state machines

Code: `crates/security/src/rotation.rs` (`Inventory`, `PolicyGate`), `KeyProvider::destroy_key`, `KeyManager::{shred_key, apply_revocation, note_rotation}`, audit ops `rotate`, `revoke`, `destroy`, and `ProviderError::Destroyed`. Tests: `crates/security/tests/rotation.rs` (15) and the lifecycle tests at the end of `crates/seal/tests/seal.rs`. Contract summary: [security.md](../security.md#implemented-rotation-revocation-and-crypto-shredding-state-machines-m0505).

## Red first

The first 14 rotation tests were run against a stub inventory and gate that accepted everything: 11 failed and the 3 that passed were the audit and shredding tests, which were written together with the provider and manager methods they call (they failed to compile before those existed). The shredding tests were then mutation-checked (below).

## Histories

`every_history_of_five_steps_...` enumerates **every** sequence of five steps over eleven moves (stage, verify and publish for each of two objects, pin a reader, release it, retire the old key version, collect an object, restart): 161,051 histories, each run against a separate model written from the rules alone. After every step it checks that each object's selection matches the model, that no selection and no live reader holds a retired version, that retiring succeeds exactly when the model says nothing depends on the version, that publishing needs a verified overlay, that collection is refused exactly while a reader pins the object, and that a restart leaves published overlays and retirements and nothing else. This is an enumeration of a bounded world, not a proof for longer histories or more objects.

## Revocation envelope (measured on the gate, provisional because it is a model of the channel)

Revocation at t = 100 s, deliveries every 5 s until the node is cut off at every second p in 0..=100, `max_age` 30 s: in all 101 cases the node stopped serving exactly 30 s after its last delivery; the worst exposure after the revocation was 30 s (cut at t = 100). A node that receives the revocation refuses at once. The data-key cache lifetime default of 300 s is outside the plan's 60 s target for any reader that does not consult the gate; the sealed backend does, on every head and read. Provider propagation delay and real channel latency are not measured here (M15).

## Crypto-shredding and outage histories (through the sealed backend)

- After `shred_key`: the warm decorator, a cold decorator on the same manager, and a restarted node with a new manager and an empty cache all refuse every read of that index; a write to it fails; `restore` on the provider does not bring the key back; another index still reads. The destruction is one audit record with the key and generation; a destruction the provider cannot perform drops nothing and is audited with its code.
- KMS outage: a cold open fails with `Unavailable`; after the outage ends the key is fetched once; a second outage leaves reads inside the cache lifetime working with no provider call, and the first read after the lifetime fails; there is no stale fallback.
- Policy partition: a decorator whose resolver consults a `PolicyGate` serves for exactly the gate's `max_age`, refuses afterwards, resumes on a delivered newer policy and stops at once on a delivered revocation.
- Revoked cached key: covered in the M05.04 tests (a warm object is rechecked against the key manager on every use) and here for `apply_revocation` dropping one index's keys only.

## Mutation round (manual)

Inventory and gate, 15 mutants: all killed after one added test (publishing an overlay whose version was retired after staging). Manager and provider, 5 mutants: all killed but one equivalent, stated: the guard that stops `restore` from clearing a destroyed key, because the provider checks destruction before revocation anyway.

## What this does not claim

- No policy channel, no delivery of generations to nodes, and no persisted inventory: the overlay's own object, the atomic selection record, and restart recovery of the inventory from published records are the lane and manifest work of later milestones. The state machine says which step may follow which; it does not store anything.
- Concurrent rewrap and garbage collection are modelled as interleaved single steps by one thread, not run on real concurrent actors against a store.
- DEK and cipher rotation (migration through compaction) is M08's.
- `destroy_key` is implemented only by the test provider; real providers (scheduled deletion, grace periods) are M15's, and a provider that only schedules deletion must not report success until the key is unusable.
- x86_64 only; no ARM run.
