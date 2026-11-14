# M03.04 authority, pins and GC model

`provisional`, **modelled, not measured**: exhaustive breadth-first search over every interleaving inside the bounds below, in `docs/plan/tools/authority_model.py` (tests: `tools/tests/test_authority_model.py`, 21 tests). It is a model of the protocol in [architecture.md](../architecture.md) ("Identities and authority"), not a proof about the Rust code, the object-store adapter or any provider. The section between the markers is the output of `python3 docs/plan/tools/authority_model.py --wide --markdown` and is regenerated, not typed. `--selftest` fails unless every correct model is safe **and every seeded control is refused**; a control that passes would mean the check or the bound cannot see the bug.

## What was run

| claim | command | result |
|---|---|---|
| all models and controls | `python3 docs/plan/tools/authority_model.py --selftest` | PASS (about 10 s) |
| including the 3-instance checkpoint run | `python3 docs/plan/tools/authority_model.py --wide --markdown` | 2,780,069 states, safe, about 45 s on a 4-vCPU sandbox |
| unit tests | `python3 -m unittest tools/tests/test_authority_model.py` | 21 pass |

## Models, bounds and what each abstracts

| model | covers | bounds |
|---|---|---|
| `lane_authority` | own-term establishment (marker create, then head CAS), several owners and epochs, same-term reopen (a new instance and lane that seals its predecessor), suspended and crashed writers with PUTs in flight at any later time, a lost reply (unknown outcome) poisoning the instance, retained fences and tail locators in the head | 3 instances, 4 slots a lane (marker plus 3), 3 records, one append in flight, 21,818 states |
| `lane_authority` with checkpoint and read | a bounded two-record checkpoint page set written before one head CAS, stale checkpointers, reclamation of covered records, fences never reclaimed, authoritative read barrier against a cached head | 2 instances, 3 records, 2 checkpoints, 1 read, 574,887 states; **and** 3 instances with 1 checkpoint, 2,780,069 states |
| `pin_gc` | pin registered, boundary re-read, GC boundary published before the pin scan, deletion below boundary and pins, stale compactors publishing by CAS | generations 0..3, 2 readers, 2 compactors, 197,649 states |
| `membership` | grouped membership with a size ceiling, joins by CAS, a full group sending the index to its own lane | cap 3, 2 joiners (12 states; deliberately tiny and separate from `promotion`) |
| `promotion` | hot-stream promotion out of a group: SEAL(X) committed in the group's ordered lane, activation head S to SEALED to T by CAS, target imports the sealed prefix before activation, every CAS may succeed, fail, or be lost with or without applying (unknown outcome), clients route by a possibly stale descriptor, a lane-mate keeps writing, two racing promoters | 2 promoters, 2 X writes, 1,695 states |
| `keys` | the M03.09 `revocation_search` histories, plus replay across indexes and positions (AAD), key rotation, a revoked-then-reinstated key, three recovery policies | histories of 6 operations ending in recovery, 10 operations, 100,000 histories a case (59,049 for the default key, which has one fewer operation) |

Safety invariants checked in every reachable state: **no acknowledged record is lost** (everything acknowledged is recovered by a fresh recoverer from the head, the checkpoint pages and the lanes), **the head never references a missing page**, **an authoritative read returns everything acknowledged before it began**, **a reader holding a generation never loses its objects and the head never names deleted objects**, **a group never exceeds its ceiling and never forgets a committed member**, **an acknowledged X write is recoverable from the authority the head names and a lane-mate's writes are never lost by a promotion**, and for keys **a record is applied exactly when its own key is not revoked, a replayed block is never applied, and recovery never fails the lane**.

## What the search found

- **A real hazard in the protocol as first written (the checkpoint model refused its own "correct" variant).** After a checkpoint covers a lane prefix and reclamation deletes those records, the first absent slot of the lane is a reclaimed hole, so a successor that seals "at the first absent slot" fences the wrong place and loses an acknowledged record that a late PUT landed after the checkpoint. The rule is now part of the model and the control `BUG_seal_looks_for_the_tail_below_the_cover` pins it: **a successor seals at the first absent slot at or above the checkpoint cover**, never from slot 0. M03.08 and M06 must implement and test this (it is a production rule, not a model detail).
- **Fences must be retained, not only the tail locator.** A late PUT into a reclaimed fence slot succeeds and is acknowledged to a client yet is invisible to recovery, which stops at the recorded tail. The head's tail locator alone does not stop the acknowledgement; the retained fence object does (`BUG_reclaim_deletes_fences`).
- **An unknown outcome of a cross-head CAS must be resolved by reading, then rolled forward.** The control `BUG_rollback_over_a_seal_after_an_unknown_outcome` reopens the source after any unknown outcome and loses an acknowledged write; strictly it shows that a rollback over a SEAL marker is unsafe, which is why the correct model has no rollback. The model has no rollback at all: a stalled promotion stays SEALED (unavailable) and never has two authorities. A rollback protocol, if ever wanted, needs its own model.
- **A stale descriptor never grants authority.** The source lane writer refuses an X write after SEAL(X) by order, not by trusting the descriptor (`BUG_descriptor_grants_write_authority`).
- **The M03.09 revocation histories pass on this model**, with the control failing exactly as M03.09 recorded: a revocable key wrapping a shared lane makes a lane-mate's records unreadable after `revoke_a` (14 histories into this larger alphabet, the same shape as M03.09's 9). `lane_model.revocation_search` and this model agree on every base verdict (the tests run both). Added families: no AAD, AAD without the index (a copy of a's block kept at its own position is applied as b: `replay_a_in_place_as_b`) and AAD without the position (a replayed block is applied twice) each fail, so both halves of "index and position" are shown necessary; a revocation that covers only the current key version after a rotation leaves old blocks readable; a recovery that drops the blocks of a revoked key loses them for good when the key is reinstated, so recovery must **skip and keep**.

## Production mapping

| model rule | production owner |
|---|---|
| marker create then head CAS before serving; successor seals every predecessor then CASes the inventory; blind head writes are refused | M03.08 lane, M06.02 |
| one append in flight, poison on any conflict or lost reply, resolve the exact slot before anything else | M03.08 lane writer |
| seal at the first absent slot at or above `cover`; fences and the tail locator retained; reclaim only after the checkpoint CAS; checkpoint pages before the head CAS | M03.08 recovery and checkpoint, M06.04 |
| authoritative read re-reads the head, never serves a cached head as latest | M07.05 barrier |
| reclamation of a reader's generation goes through the pin protocol (`pin_gc`), keyed by checkpoint version | M03.08, M07 |
| promotion: SEAL(X) in the source order, target imports the sealed prefix, activation CAS from SEALED only, resolve unknown, never roll back | M03.08 hot promotion |
| record AAD binds index and position; recovery skips and keeps blocks whose key is revoked | M03.03 format (AAD field is in the format inputs), M05 |

## Limits and downsides

- **Bounded, not unbounded.** The counts above are the whole claim: a bug needing a fourth instance, a fifth record, a second checkpoint in the wide run or a third promoter is not excluded. Small bounds found every seeded bug, which is the evidence the bounds can see this class, not that the class is exhausted.
- **Private lanes, no contended establishment object.** Each instance writes only its own lane, so the marker create never conflicts and a same-term reopen is just a new instance sealing its predecessor and winning the head CAS; two instances racing to create the same lane object are not modelled. **Stale descriptors** are modelled only as non-deterministic routing of a write to the wrong domain (the target must refuse by order), not as independently cached descriptor state. The authoritative read is one atomic scan after a head snapshot: it checks head staleness, not a scan racing PUTs, reclaim or checkpoints (reclaim is disabled from a read's snapshot until its scan).
- **Abstractions.** One writer per lane with at most one append in flight; the provider is a linearizable create-if-absent and CAS with at-least the stated reply loss (no stale reads, no phantom creates, no partial objects); time, leases and clocks are absent (they affect liveness only, by design); object bytes, sizes and request counts are not modelled (the cost side is `lane_model.py`).
- **Reclamation versus a reader** in the lane model is excluded by disabling reclamation while a read is in flight; the real guarantee is `pin_gc`'s, and the two models are joined only by this sentence and the mapping above. Pin expiry is not modelled: a crashed reader's pin blocks GC until it expires, and a pin that expires under a live reader is a safety hole the M07 reader must close by revalidating.
- **Keys are symbolic.** AEAD is an equality check, the adversary only copies whole blocks, a block failing authentication is skipped (an availability choice; failing the lane would let one forged block stop its lane-mates, which M05 must decide knowingly), and key rotation is a version counter, not a rewrap.
- **Not modelled:** quorum mode, cross-region replication, schema/publication generations in the head, GC of the idempotency window, multiple groups promoting in parallel, an index joining and leaving a group concurrently with its promotion (membership and promotion are separate models), and any Rust-level or provider-level behaviour.
- A 2.8M-state run takes about 45 s in pure Python: acceptable as an evidence run, too slow for `selftest`, which is why only the 2-instance configuration is in the tests.

## Generated results

<!-- generated:begin -->
### lane_authority

| case | verdict | states / histories | counterexample (shortest) |
|---|---|---|---|
| correct | safe | 21,818 | |
| BUG_two_appends_in_flight | **unsafe** (expected) | 228 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 0 publishes itself in the head (CAS on version 0) → instance 0 issues record 0 for slot 1 → instance 0 issues record 1 for slot 2 → PUT of record 1 to lane 0 slot 2 succeeds, reply delivered — acknowledged record(s) [1] are not recovered |
| BUG_continue_after_failure | **unsafe** (expected) | 1,378 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 0 publishes itself in the head (CAS on version 0) → instance 0 issues record 0 for slot 1 → instance 1 reads the head (version 1) → instance 1 seals lane 0 at slot 1 → PUT of record 0 to lane 0 slot 1 collides with 'F' → instance 0 issues record 1 for slot 2 → PUT of record 1 to lane 0 slot 2 succeeds, reply delivered — acknowledged record(s) [1] are not recovered |
| BUG_seal_recorded_without_a_fence | **unsafe** (expected) | 1,339 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 0 publishes itself in the head (CAS on version 0) → instance 0 issues record 0 for slot 1 → instance 1 reads the head (version 1) → instance 1 seals lane 0 at slot 1 WITHOUT a fence → instance 1 creates its establishment marker → instance 1 publishes itself in the head (CAS on version 1) → PUT of record 0 to lane 0 slot 1 succeeds, reply delivered — acknowledged record(s) [0] are not recovered |
| BUG_serve_before_establishing | **unsafe** (expected) | 68 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 0 issues record 0 for slot 1 → PUT of record 0 to lane 0 slot 1 succeeds, reply delivered — acknowledged record(s) [0] are not recovered |
| BUG_blind_head_write | **unsafe** (expected) | 932 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 1 reads the head (version 0) → instance 0 publishes itself in the head (CAS on version 0) → instance 0 issues record 0 for slot 1 → instance 1 creates its establishment marker → instance 1 publishes itself in the head (CAS on version 0, blind) → PUT of record 0 to lane 0 slot 1 succeeds, reply delivered — acknowledged record(s) [0] are not recovered |

### lane_checkpoint_and_read

| case | verdict | states / histories | counterexample (shortest) |
|---|---|---|---|
| correct | safe | 574,887 | |
| BUG_reclaim_before_publishing | **unsafe** (expected) | 400 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 0 publishes itself in the head (CAS on version 0) → instance 0 issues record 0 for slot 1 → PUT of record 0 to lane 0 slot 1 succeeds, reply delivered → instance 0 snapshots a checkpoint of [0] at head version 1 → reclaim lane 0 slot 1 (('R', 0)) — acknowledged record(s) [0] are not recovered |
| BUG_reclaim_deletes_fences | **unsafe** (expected) | 9,515 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 0 publishes itself in the head (CAS on version 0) → instance 0 issues record 0 for slot 1 → instance 0 snapshots a checkpoint of [] at head version 1 → instance 0 publishes the checkpoint (CAS on version 1) → instance 1 reads the head (version 2) → instance 1 seals lane 0 at slot 1 → instance 1 creates its establishment marker → instance 1 publishes itself in the head (CAS on version 2) → reclaim lane 0 slot 1 ('F') → PUT of record 0 to lane 0 slot 1 succeeds, reply delivered — acknowledged record(s) [0] are not recovered |
| BUG_seal_looks_for_the_tail_below_the_cover | **unsafe** (expected) | 59,588 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 0 publishes itself in the head (CAS on version 0) → instance 0 issues record 0 for slot 1 → PUT of record 0 to lane 0 slot 1 succeeds, reply delivered → instance 0 issues record 1 for slot 2 → instance 0 snapshots a checkpoint of [0] at head version 1 → instance 0 writes checkpoint page 0 → instance 0 publishes the checkpoint (CAS on version 1) → instance 1 reads the head (version 2) → PUT of record 1 to lane 0 slot 2 succeeds, reply delivered → reclaim lane 0 slot 1 (('R', 0)) → instance 1 seals lane 0 at slot 1 → instance 1 creates its establishment marker → instance 1 publishes itself in the head (CAS on version 2) — acknowledged record(s) [1] are not recovered |
| BUG_blind_checkpoint_write | **unsafe** (expected) | 6,546 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 0 publishes itself in the head (CAS on version 0) → instance 0 snapshots a checkpoint of [] at head version 1 → instance 1 reads the head (version 1) → instance 1 seals lane 0 at slot 1 → instance 1 creates its establishment marker → instance 1 publishes itself in the head (CAS on version 1) → instance 0 publishes the checkpoint (CAS on version 1, blind) → instance 1 issues record 0 for slot 1 → PUT of record 0 to lane 1 slot 1 succeeds, reply delivered — acknowledged record(s) [0] are not recovered |
| BUG_head_before_pages | **unsafe** (expected) | 400 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 0 publishes itself in the head (CAS on version 0) → instance 0 issues record 0 for slot 1 → PUT of record 0 to lane 0 slot 1 succeeds, reply delivered → instance 0 snapshots a checkpoint of [0] at head version 1 → instance 0 publishes the checkpoint (CAS on version 1) — the head references a checkpoint page that does not exist |
| BUG_cached_head_read_as_latest | **unsafe** (expected) | 427 | instance 0 reads the head (version 0) → instance 0 creates its establishment marker → instance 0 publishes itself in the head (CAS on version 0) → instance 0 issues record 0 for slot 1 → PUT of record 0 to lane 0 slot 1 succeeds, reply delivered → reader snapshots the head (version 0), possibly from a cache → reader scans the lanes from that head — the read missed record(s) [0] acknowledged before it began |
| correct, three instances, one checkpoint | safe | 2,780,069 | |

### pin_gc

| case | verdict | states / histories | counterexample (shortest) |
|---|---|---|---|
| correct | safe | 197,649 | |
| BUG_pin_not_rechecked_against_boundary | **unsafe** (expected) | 1,614 | reader 0 reads the head (generation 0) → compactor 0 snapshots the head (generation 0) → compactor 0 writes the objects of generation 1 → compactor 0 publishes generation 1 (CAS on 0) → GC publishes boundary 1 → GC reads the pin registry → reader 0 registers a pin on generation 0 → reader 0 re-reads the boundary (1) and proceeds → GC deletes generation 0 — a reader holding generation 0 lost its objects |
| BUG_gc_ignores_pins | **unsafe** (expected) | 1,298 | reader 0 reads the head (generation 0) → reader 0 registers a pin on generation 0 → reader 0 re-reads the boundary (0) and proceeds → compactor 0 snapshots the head (generation 0) → compactor 0 writes the objects of generation 1 → compactor 0 publishes generation 1 (CAS on 0) → GC publishes boundary 1 → GC reads the pin registry → GC deletes generation 0 — a reader holding generation 0 lost its objects |
| BUG_gc_scans_pins_before_publishing_boundary | **unsafe** (expected) | 1,816 | reader 0 reads the head (generation 0) → compactor 0 snapshots the head (generation 0) → compactor 0 writes the objects of generation 1 → compactor 0 publishes generation 1 (CAS on 0) → GC reads the pin registry (before any boundary) → reader 0 registers a pin on generation 0 → reader 0 re-reads the boundary (0) and proceeds → GC publishes boundary 1 → GC deletes generation 0 — a reader holding generation 0 lost its objects |
| BUG_blind_publication | **unsafe** (expected) | 26,382 | compactor 0 snapshots the head (generation 0) → compactor 0 writes the objects of generation 1 → compactor 1 snapshots the head (generation 0) → compactor 0 publishes generation 1 (CAS on 0, blind) → compactor 0 snapshots the head (generation 1) → compactor 0 writes the objects of generation 2 → compactor 0 publishes generation 2 (CAS on 1, blind) → compactor 1 writes the objects of generation 1 → GC publishes boundary 2 → compactor 1 publishes generation 1 (CAS on 0, blind) → GC reads the pin registry → GC deletes generation 0 → GC deletes generation 1 — the head names generation 1, whose objects were deleted |

### membership

| case | verdict | states / histories | counterexample (shortest) |
|---|---|---|---|
| correct | safe | 12 | |
| BUG_blind_membership_write | **unsafe** (expected) | 12 | Z0 reads the group head → Z1 reads the group head → Z0 joins (CAS on version 0, blind) → Z1 joins (CAS on version 0, blind) — member(s) ['Z0'] were committed and then forgotten |
| BUG_join_ignores_the_ceiling | **unsafe** (expected) | 11 | Z0 reads the group head → Z0 joins (CAS on version 0) → Z1 reads the group head → Z1 joins (CAS on version 1) — the group has 4 members, above its ceiling 3 |

### promotion

| case | verdict | states / histories | counterexample (shortest) |
|---|---|---|---|
| correct | safe | 1,695 | |
| BUG_import_before_the_seal | **unsafe** (expected) | 1,175 | promoter 0 reads the activation head (S) → promoter 0 imports X's records before sealing → X write 0 routed to S is acknowledged → promoter 0 appends SEAL(X) to S → promoter 0 CAS S->SEALED applies, reply delivered → promoter 0 finds T already holds a prefix → promoter 0 CAS SEALED->T applies, reply delivered — acknowledged X write(s) [0] are not recoverable from the authority the head names |
| BUG_descriptor_grants_write_authority | **unsafe** (expected) | 31 | promoter 0 reads the activation head (S) → promoter 0 appends SEAL(X) to S → X write 0 routed to S is acknowledged — acknowledged X write(s) [0] are not recoverable from the authority the head names |
| BUG_rollback_over_a_seal_after_an_unknown_outcome | **unsafe** (expected) | 200 | promoter 0 reads the activation head (S) → promoter 0 appends SEAL(X) to S → promoter 0 CAS S->SEALED applies, reply lost → promoter 0 treats the unknown outcome as failure and reopens S → X write 0 routed to S is acknowledged — acknowledged X write(s) [0] are not recoverable from the authority the head names |

### keys

| case | verdict | states / histories | counterexample (shortest) |
|---|---|---|---|
| per_index_dek, skip, authenticated index and position | safe | 100,000 | |
| lane_dek_default, skip | safe | 59,049 | |
| BUG_revocable_key_wraps_a_shared_lane | **unsafe** (expected) | 14 | append_a → append_a → append_a → append_b → revoke_a → recover — record ('b', 3) readable=False but its own key is not revoked |
| BUG_recovery_fails_the_lane | **unsafe** (expected) | 4 | append_a → append_a → append_a → append_a → revoke_a → recover — recovery failed the whole lane on a block whose key is revoked |
| BUG_recovery_drops_revoked_blocks | **unsafe** (expected) | 326 | append_a → append_a → revoke_a → recover → reinstate_a → recover — record ('a', 0) readable=False but its own key is not revoked |
| BUG_no_aad | **unsafe** (expected) | 8 | append_a → append_a → append_a → append_a → replay_a_as_b → recover — a block of index a was applied as index b |
| BUG_aad_without_the_index | **unsafe** (expected) | 10 | append_a → append_a → append_a → append_a → replay_a_in_place_as_b → recover — a block of index a was applied as index b |
| BUG_aad_without_the_position | **unsafe** (expected) | 9 | append_a → append_a → append_a → append_a → replay_a_again → recover — a replayed block was applied twice |
| BUG_revoke_only_the_current_key_version | **unsafe** (expected) | 64 | append_a → append_a → append_a → rotate_a → revoke_a → recover — record ('a', 0) readable=True but its own key is revoked |
<!-- generated:end -->
