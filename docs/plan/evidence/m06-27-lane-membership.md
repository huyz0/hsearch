# M06.27 lane membership and reclaim wiring

`provisional`: in-memory object store on one Linux host.

## What exists

- **`member::create`** (`crates/store/src/member.rs`): resolves the name in the catalog, binds it to
  `index_id_in(name, incarnation)` (the incarnation is the catalog's name generation, which rises
  with every bind and every delete), then joins the index to the lane in one descriptor CAS. A
  repeated create returns the same member and finishes a join that stopped short.
- **`member::delete`**: the lane seals the index at its last sequence and drops its membership
  (`Committer::leave`, repeatable after a lost reply), the pins forget it, and the catalog leaves
  the tombstone that keeps the old identity from being bound again.
- **`member::reclaim`**: raises the head's cover to what the pins allow (one head read, one CAS when
  it moves), then the lane writer removes the slots below it. No request names an index.
- **`member::settle_tail` / `settle_overdue`**: publish the indexes a lane's tail left pinned when it
  opens, and the ones whose pin held the span bound after a commit; an index that does not publish
  (failed, evicted, not open) stays pinned and holds the cover.
- **Lane thread**: `Log` gained `leave` and `reclaim`; the shared lane thread runs both as jobs in
  order with commits, so every handle sees the seal and the removal and the single writer does them.

## Evidence

`crates/store/tests/member.rs` (5): create binds incarnation and membership and repeats to the same
member; a deleted index stops pinning, leaves the members, is refused writes and cannot be deleted
twice, and its recreate is a new identity that replays none of the old records and repeats stably;
reclaim removes nothing while the first records are pinned and removes what the cover passed once
published; leave and reclaim through the lane thread are seen by every handle; a tail index that does
not publish stays pinned and keeps the cover at its record, and an overdue pin is settled.

Mutations seen red: the pins' forget, the cover advance before reclaim, the incarnation increment,
the membership removal, the seal.

## Limits

- The node's own call sites (its create and delete routes opening hosts over a shared lane thread,
  and its commit path calling `settle_overdue`) are the integration of M06.21, which this task
  unblocks: `ServedIndex` still serves a dedicated lane.
- The per-restart request count is not re-measured here: M06.26 asserts a restart with every index
  published reads no slot.
