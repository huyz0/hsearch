#!/usr/bin/env python3
"""M03.04: bounded explicit-state models of authority, pins, GC, promotion and key revocation.

Four small exhaustive models, each checked by breadth-first search over every interleaving inside
published bounds, and each shipped with seeded broken controls that MUST fail. A control that passes
means the check or the bound is too weak to see the bug, so `selftest()` and the unit tests refuse it.

  lane       ordered conditional creates, own-term establishment, same-term reopen, suspended and
             crashed writers with in-flight PUTs, retained fences and tail locators, bounded
             checkpoint pages, structural CAS, reclamation, authoritative read barrier.
  pin_gc     pin registration against GC boundaries, stale publishers and compactors.
  promotion  grouped membership (size ceiling), hot-stream promotion across two heads, independently
             stale descriptors, unknown outcomes of cross-head CAS.
  keys       the M03.09 lane-mate key-revocation histories (`lane_model.revocation_search`) plus AAD and
             replay, key rotation and revoked-then-reinstated keys.

This is a MODEL, not a proof about the Rust code or about a provider. Bounds, abstractions and the
production mapping are stated in docs/plan/evidence/m03-04-authority-model.md and in each docstring.
"""
from __future__ import annotations

import argparse
import itertools
import json
import sys
from collections import deque
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lane_model  # noqa: E402

STATE_LIMIT = 4_000_000


def explore(initial, steps, invariant, limit=STATE_LIMIT):
    """Breadth-first search. `steps(state)` yields (label, next_state); `invariant(state)` returns
    None or a message. The first violation found is a shortest counterexample. Hitting the state
    limit raises instead of reporting safe: an unfinished search proves nothing."""
    parent = {initial: None}
    message = invariant(initial)
    if message:
        return {"safe": False, "states": 1, "counterexample": [], "violation": message}
    queue, terminal = deque([initial]), 0
    while queue:
        state = queue.popleft()
        moved = False
        for label, nxt in steps(state):
            moved = True
            if nxt in parent:
                continue
            parent[nxt] = (state, label)
            message = invariant(nxt)
            if message:
                path, cur = [], nxt
                while parent[cur] is not None:
                    cur, lab = parent[cur][0], parent[cur][1]
                    path.append(lab)
                return {"safe": False, "states": len(parent), "counterexample": path[::-1], "violation": message}
            if len(parent) > limit:
                raise RuntimeError(f"state limit {limit} exceeded: the search is unfinished, nothing is claimed")
            queue.append(nxt)
        if not moved:
            terminal += 1
    return {"safe": True, "states": len(parent), "terminal_states": terminal}


def put(items, index, value):
    return items[:index] + (value,) + items[index + 1:]


# --------------------------------------------------------------------------------------------------
# lane: authority, fences, checkpoints, read barrier
# --------------------------------------------------------------------------------------------------
# One lane object per instance (slot 0 is its own establishment marker "E", data from slot 1). An
# instance is one run of a writer: a restart in the same term is a new instance. The structural head
# is one conditionally updated object: (version, inventory, sealed_at, cover, checkpoint pages).
#   inventory   instance lanes in order; every one but the last is sealed at the recorded slot (the
#               tail locator); recovery reads a lane from `cover` up to that slot.
#   cover       per lane, the slots a published checkpoint already contains (slots below may be
#               reclaimed; a fence and the tail locator are never reclaimed).
#   checkpoint  page keys; every page holds at most PAGE records, published before the head CAS.
# Slot values: None absent, ("R", id) a record, "E" establishment marker, "F" fence.
# Abstraction: one writer per lane with at most PIPE appends in flight; a PUT either takes effect
# with a reply, takes effect with a lost reply (unknown outcome) or collides; a delivered PUT may
# arrive at any later time, even after its instance died (suspended writer, old incarnation).
# Not modelled: provider anomalies other than reply loss, clocks and leases (liveness only), key
# material, request counts, and reclamation racing a reader (that is `pin_gc`; here reclamation is
# disabled while a read is in flight, which the production mapping must provide through pins).
LANE_BOUNDS = {"instances": 3, "slots_per_lane": 4, "records": 3, "page_records": 2,
               "checkpoints": 2, "reads": 1, "in_flight_per_instance": 1}

EMPTY_HEAD = (0, (), (), (0, 0, 0), ())
# instance: (status, snapshot head, sealed_at of the lane it sealed, next slot, in flight, poisoned, checkpoint)
IDLE = ("idle", None, -1, 0, 0, False, None)


def _recs(entries):
    return {e[1] for e in entries if isinstance(e, tuple)}


def _lane_records(lane, start, stop, slots):
    out, s = set(), start
    while s < min(stop, slots):
        entry = lane[s]
        if isinstance(entry, tuple):
            out.add(entry[1])
        elif entry != "E":
            break
        s += 1
    return out, s


def lane_recover(head, lanes, pages, slots):
    """What a fresh recoverer reads: checkpoint pages plus every inventory lane from its cover. The
    last lane is read to its first absent slot (the recoverer would fence it there)."""
    got, page_map = set(), dict(pages)
    for key in head[4]:
        if key not in page_map:
            return None, "the head references a checkpoint page that does not exist"
        got |= set(page_map[key])
    inv, sealed, cover = head[1], head[2], head[3]
    for position, lane_id in enumerate(inv):
        stop = sealed[position] if sealed[position] >= 0 else slots
        read, _ = _lane_records(lanes[lane_id], cover[lane_id], stop, slots)
        got |= read
    return got, None


def lane_authority(pipeline=1, poison=True, fence_on_seal=True, establish_before_serving=True,
               versioned_head=True, checkpoint=False, reads=False, cached_read=False,
               reclaim_after_publish=True, reclaim_fences=False, versioned_checkpoint=True,
               pages_before_head=True, seal_from_cover=True, instances=3, slots=4, records=3, checkpoints=2):
    """Seeded controls flip one argument each: pipeline=2, poison=False,
    fence_on_seal=False, establish_before_serving=False, versioned_head=False, cached_read=True,
    reclaim_after_publish=False, reclaim_fences=True, versioned_checkpoint=False, pages_before_head=False, seal_from_cover=False."""
    page_cap = LANE_BOUNDS["page_records"]
    cover0 = (0,) * instances
    head0 = (0, (), (), cover0, ())
    lanes0 = tuple((None,) * slots for _ in range(instances))
    # (lanes, head, history, pages, instances, inflight, acked, next record, checkpoints run, read, reads run, violation)
    initial = (lanes0, head0, (head0,), frozenset(), (IDLE,) * instances, frozenset(), frozenset(), 0, 0, None, 0, None)

    def invariant(state):
        lanes, head, _, pages, _, _, acked, _, _, _, _, violation = state
        if violation:
            return violation
        got, error = lane_recover(head, lanes, pages, slots)
        if error:
            return error
        lost = acked - got
        if lost:
            return f"acknowledged record(s) {sorted(lost)} are not recovered"
        return None

    def sealed_prefix_end(lane):
        s = 0
        while s < slots and (lane[s] == "E" or isinstance(lane[s], tuple)):
            s += 1
        return s

    def steps(state):
        lanes, head, hist, pages, inst, inflight, acked, nextrec, ck_runs, rd, rd_runs, violation = state
        for i in range(instances):
            status, snap, sealed_at, nslot, pend, poisoned, ck = inst[i]
            if status != "dead":
                yield f"crash instance {i}", state[:4] + (put(inst, i, ("dead", snap, sealed_at, nslot, pend, poisoned, ck)),) + state[5:]
            if status == "idle" and (i == 0 or inst[i - 1][0] != "idle"):
                nxt = "sealing" if head[1] else "sealed"
                yield f"instance {i} reads the head (version {head[0]})", state[:4] + (put(inst, i, (nxt, head, -1, 0, 0, False, None)),) + state[5:]
            if status == "sealing":
                last = snap[1][-1]
                lane = lanes[last]
                start = snap[3][last] if seal_from_cover else 0
                tail = lane[start:]
                if "F" in tail:
                    at, new_lane = start + tail.index("F"), lane
                elif None in tail:
                    at = start + tail.index(None)
                    new_lane = put(lane, at, "F") if fence_on_seal else lane
                else:
                    at, new_lane = slots, lane
                yield (f"instance {i} seals lane {last} at slot {at}" + ("" if fence_on_seal else " WITHOUT a fence"),
                       (put(lanes, last, new_lane),) + state[1:4] + (put(inst, i, ("sealed", snap, at, 0, 0, False, None)),) + state[5:])
            if status == "sealed":
                yield f"instance {i} creates its establishment marker", (put(lanes, i, put(lanes[i], 0, "E")),) + state[1:4] + (put(inst, i, ("marked", snap, sealed_at, 1, 0, False, None)),) + state[5:]
            if status == "marked":
                current_ok = head[0] == snap[0]
                if current_ok or not versioned_head:
                    old = snap if not current_ok else head
                    sealed_old = tuple(old[2][:-1]) + (sealed_at,) if old[1] else ()
                    new_head = (head[0] + 1, old[1] + (i,), sealed_old + (-1,), head[3], head[4])
                    nxt_hist = hist + ((new_head,) if new_head not in hist else ())
                    yield (f"instance {i} publishes itself in the head (CAS on version {snap[0]}{'' if current_ok else ', blind'})",
                           state[:1] + (new_head, nxt_hist) + state[3:4] + (put(inst, i, ("serving", snap, sealed_at, nslot, pend, poisoned, ck)),) + state[5:])
                else:
                    yield f"instance {i} loses the head CAS and retires", state[:4] + (put(inst, i, ("dead", snap, sealed_at, nslot, pend, poisoned, ck)),) + state[5:]
            serving = status == "serving" or (status == "marked" and not establish_before_serving)
            if serving and (not poisoned or not poison) and pend < pipeline and nextrec < records and nslot < slots:
                yield (f"instance {i} issues record {nextrec} for slot {nslot}",
                       state[:4] + (put(inst, i, (status, snap, sealed_at, nslot + 1, pend + 1, poisoned, ck)), inflight | {(i, nslot, nextrec)}, acked, nextrec + 1) + state[8:])
            if checkpoint and status == "serving" and ck is None and ck_runs < checkpoints and not poisoned:
                cover2, recs = list(head[3]), set()
                got, error = lane_recover(head, lanes, pages, slots)
                if not error:
                    for position, lane_id in enumerate(head[1]):
                        stop = head[2][position] if head[2][position] >= 0 else slots
                        _, end = _lane_records(lanes[lane_id], head[3][lane_id], stop, slots)
                        cover2[lane_id] = end
                    recs = set(got)
                    # the records below the new cover that are already in `got` plus any in the extended range
                    for position, lane_id in enumerate(head[1]):
                        stop = head[2][position] if head[2][position] >= 0 else slots
                        read, _ = _lane_records(lanes[lane_id], head[3][lane_id], min(stop, cover2[lane_id]), slots)
                        recs |= read
                    ordered = tuple(sorted(recs))
                    yield (f"instance {i} snapshots a checkpoint of {list(ordered)} at head version {head[0]}",
                           state[:4] + (put(inst, i, (status, snap, sealed_at, nslot, pend, poisoned, (head, tuple(cover2), ordered, 0))), ) + state[5:8] + (ck_runs + 1,) + state[9:])
            if ck is not None and status != "dead":
                h0, cover2, ordered, done = ck
                npages = -(-len(ordered) // page_cap)
                if done < npages:
                    key = (i, h0[0], done)
                    content = ordered[done * page_cap:(done + 1) * page_cap]
                    yield (f"instance {i} writes checkpoint page {done}",
                           state[:3] + (pages | {(key, content)}, put(inst, i, (status, snap, sealed_at, nslot, pend, poisoned, (h0, cover2, ordered, done + 1)))) + state[5:])
                if done == npages or not pages_before_head:
                    keys = tuple((i, h0[0], n) for n in range(npages))
                    if head[0] == h0[0] or not versioned_checkpoint:
                        base = h0 if not versioned_checkpoint else head
                        new_head = (head[0] + 1, base[1], base[2], cover2, keys)
                        yield (f"instance {i} publishes the checkpoint (CAS on version {h0[0]}{'' if versioned_checkpoint else ', blind'})",
                               state[:1] + (new_head, hist + ((new_head,) if new_head not in hist else ())) + state[3:4] + (put(inst, i, (status, snap, sealed_at, nslot, pend, poisoned, None)),) + state[5:])
                    else:
                        yield f"instance {i} loses the checkpoint CAS", state[:4] + (put(inst, i, (status, snap, sealed_at, nslot, pend, poisoned, None)),) + state[5:]
        for (i, slot, rec) in inflight:
            status, snap, sealed_at, nslot, pend, poisoned, ck = inst[i]
            rest = inflight - {(i, slot, rec)}
            if lanes[i][slot] is None:
                written = put(lanes, i, put(lanes[i], slot, ("R", rec)))
                live = status != "dead"
                yield (f"PUT of record {rec} to lane {i} slot {slot} succeeds, reply delivered" + ("" if live else " to a dead writer"),
                       (written,) + state[1:4] + (put(inst, i, (status, snap, sealed_at, nslot, pend - 1, poisoned, ck)), rest, acked | ({rec} if live else frozenset())) + state[7:])
                yield (f"PUT of record {rec} to lane {i} slot {slot} takes effect, reply lost",
                       (written,) + state[1:4] + (put(inst, i, (status, snap, sealed_at, nslot, pend - 1, poison, ck)), rest) + state[6:])
            else:
                yield (f"PUT of record {rec} to lane {i} slot {slot} collides with {lanes[i][slot]!r}",
                       state[:4] + (put(inst, i, (status, snap, sealed_at, nslot, pend - 1, poison, ck)), rest) + state[6:])
        if checkpoint:
            busy = rd is not None and rd != "done"
            for lane_id in range(instances):
                for s in range(slots):
                    entry = lanes[lane_id][s]
                    if entry is None or busy:
                        continue
                    in_cover = head[3][lane_id] > s
                    early = (not reclaim_after_publish) and any(c is not None and c[1][lane_id] > s for _, _, _, _, _, _, c in inst)
                    fence_dropped = reclaim_fences and entry == "F" and head[3][lane_id] >= s > 0 and lane_id in head[1]
                    removable = (isinstance(entry, tuple) and (in_cover or early)) or fence_dropped
                    if removable:
                        yield f"reclaim lane {lane_id} slot {s} ({entry!r})", (put(lanes, lane_id, put(lanes[lane_id], s, None)),) + state[1:]
        if reads and rd is None and rd_runs < LANE_BOUNDS["reads"]:
            options = {head} if not cached_read else set(hist)
            for snap_head in sorted(options):
                yield (f"reader snapshots the head (version {snap_head[0]})" + ("" if not cached_read else ", possibly from a cache"),
                       state[:9] + ((acked, snap_head), rd_runs + 1, violation))
        if rd is not None and rd != "done":
            acked_at_start, snap_head = rd
            got, error = lane_recover(snap_head, lanes, pages, slots)
            if error:
                bad = error
            else:
                missing = acked_at_start - got
                bad = f"the read missed record(s) {sorted(missing)} acknowledged before it began" if missing else None
            yield "reader scans the lanes from that head", state[:9] + ("done", rd_runs, bad)

    return explore(initial, steps, invariant)


LANE_CONTROLS = {
    "BUG_two_appends_in_flight": dict(pipeline=2),
    "BUG_continue_after_failure": dict(poison=False),
    "BUG_seal_recorded_without_a_fence": dict(fence_on_seal=False),
    "BUG_serve_before_establishing": dict(establish_before_serving=False),
    "BUG_blind_head_write": dict(versioned_head=False),
}
CHECKPOINT_CONTROLS = {
    "BUG_reclaim_before_publishing": dict(reclaim_after_publish=False),
    "BUG_reclaim_deletes_fences": dict(reclaim_fences=True),
    "BUG_seal_looks_for_the_tail_below_the_cover": dict(seal_from_cover=False),
    "BUG_blind_checkpoint_write": dict(versioned_checkpoint=False),
    "BUG_head_before_pages": dict(pages_before_head=False),
}
READ_CONTROLS = {"BUG_cached_head_read_as_latest": dict(cached_read=True)}


# --------------------------------------------------------------------------------------------------
# pin_gc: pins against GC boundaries, stale publishers
# --------------------------------------------------------------------------------------------------
# Generations 0..G; the head names one generation, a publication CAS moves it. A reader pins the
# generation it read, then re-reads the GC boundary and abandons the pin if the boundary passed it.
# GC publishes a boundary first, then reads the pin registry, then deletes generations below the
# boundary and below every pin. Not modelled: pin expiry (a crashed reader's pin blocks GC forever;
# production needs a TTL and a reader that revalidates, which is a liveness-versus-safety choice
# M03.08/M07 must make), and the cost of the registry scan.
def pin_gc(recheck_boundary=True, gc_reads_pins=True, intent_before_scan=True, versioned_publish=True,
           generations=3, readers=2, compactors=2):
    """Controls: recheck_boundary=False, gc_reads_pins=False, intent_before_scan=False, versioned_publish=False."""
    # (head, live, boundary, pins, readers, gc, compactors)
    initial = (0, frozenset({0}), 0, frozenset(), (("idle", None),) * readers, ("idle", None, None), (("idle", None),) * compactors)

    def invariant(state):
        head, live, boundary, pins, rds, _, _ = state
        if head not in live:
            return f"the head names generation {head}, whose objects were deleted"
        for status, g in rds:
            if status == "reading" and g not in live:
                return f"a reader holding generation {g} lost its objects"
        return None

    def steps(state):
        head, live, boundary, pins, rds, gc, comps = state
        for r, (status, g) in enumerate(rds):
            if status == "idle":
                yield f"reader {r} reads the head (generation {head})", state[:4] + (put(rds, r, ("snapped", head)),) + state[5:]
            elif status == "snapped":
                yield f"reader {r} registers a pin on generation {g}", state[:3] + (pins | {(r, g)}, put(rds, r, ("pinned", g))) + state[5:]
            elif status == "pinned":
                if not recheck_boundary or boundary <= g:
                    yield f"reader {r} re-reads the boundary ({boundary}) and proceeds", state[:4] + (put(rds, r, ("reading", g)),) + state[5:]
                else:
                    yield f"reader {r} sees the boundary {boundary} passed generation {g}, drops the pin", state[:3] + (pins - {(r, g)}, put(rds, r, ("done", g))) + state[5:]
            elif status == "reading":
                yield f"reader {r} finishes and unpins", state[:3] + (pins - {(r, g)}, put(rds, r, ("done", g))) + state[5:]
        for c, (status, h0) in enumerate(comps):
            if status == "idle" and head + 1 <= generations:
                yield f"compactor {c} snapshots the head (generation {head})", state[:6] + (put(comps, c, ("snapped", head)),)
            elif status == "snapped" and h0 + 1 <= generations:
                yield f"compactor {c} writes the objects of generation {h0 + 1}", (head, live | {h0 + 1}) + state[2:6] + (put(comps, c, ("built", h0)),)
            elif status == "built":
                if head == h0 or not versioned_publish:
                    yield (f"compactor {c} publishes generation {h0 + 1} (CAS on {h0}{'' if versioned_publish else ', blind'})",
                           (h0 + 1,) + state[1:6] + (put(comps, c, ("idle", None)),))
                else:
                    yield f"compactor {c} loses the publication CAS", state[:6] + (put(comps, c, ("idle", None)),)
        phase, target, seen = gc
        if phase == "idle" and head > boundary and head > 0:
            if intent_before_scan:
                yield f"GC publishes boundary {head}", state[:2] + (head,) + state[3:5] + (("intent", head, None),) + state[6:]
            else:
                yield f"GC reads the pin registry (before any boundary)", state[:5] + (("scanned_first", head, min((g for _, g in pins), default=99)),) + state[6:]
        elif phase == "intent":
            yield "GC reads the pin registry", state[:5] + (("scanned", target, min((g for _, g in pins), default=99) if gc_reads_pins else 99),) + state[6:]
        elif phase == "scanned_first":
            yield f"GC publishes boundary {target}", state[:2] + (target,) + state[3:5] + (("scanned", target, seen),) + state[6:]
        elif phase == "scanned":
            doomed = [g for g in sorted(live) if g < min(target, seen)]
            if doomed:
                yield f"GC deletes generation {doomed[0]}", (head, live - {doomed[0]}) + state[2:]
            else:
                yield "GC finishes", state[:5] + (("idle", None, None),) + state[6:]
    return explore(initial, steps, invariant)


PIN_CONTROLS = {
    "BUG_pin_not_rechecked_against_boundary": dict(recheck_boundary=False),
    "BUG_gc_ignores_pins": dict(gc_reads_pins=False),
    "BUG_gc_scans_pins_before_publishing_boundary": dict(intent_before_scan=False),
    "BUG_blind_publication": dict(versioned_publish=False),
}


# --------------------------------------------------------------------------------------------------
# promotion: grouped membership and hot-stream promotion across two heads
# --------------------------------------------------------------------------------------------------
def membership(versioned=True, check_cap=True, cap=3, joiners=2):
    """Grouped membership: a lane group head holds a bounded member set; joins are CAS. Controls:
    versioned=False (a blind overwrite from a stale snapshot), check_cap=False (a join that ignores the ceiling). Invariant: the member count never
    exceeds the ceiling and no committed member is forgotten."""
    # (head version, members, joiners, truth)
    base = frozenset({"X", "Y"})
    initial = (0, base, (("idle", None),) * joiners, base)

    def invariant(state):
        version, members, js, truth = state
        if len(members) > cap:
            return f"the group has {len(members)} members, above its ceiling {cap}"
        if not truth <= members:
            return f"member(s) {sorted(truth - members)} were committed and then forgotten"
        return None

    def steps(state):
        version, members, js, truth = state
        for j, (status, snap) in enumerate(js):
            name = f"Z{j}"
            if status == "idle":
                yield f"{name} reads the group head", state[:2] + (put(js, j, ("read", (version, members))),) + state[3:]
            elif status == "read":
                if check_cap and len(snap[1]) >= cap:
                    yield f"{name} finds the group full and gets its own lane", state[:2] + (put(js, j, ("alone", None)),) + state[3:]
                elif snap[0] == version or not versioned:
                    joined = frozenset(snap[1] | {name})
                    yield (f"{name} joins (CAS on version {snap[0]}{'' if versioned else ', blind'})",
                           (version + 1, joined, put(js, j, ("member", None)), truth | {name}))
                else:
                    yield f"{name} loses the join CAS and re-reads", state[:2] + (put(js, j, ("idle", None)),) + state[3:]
    return explore(initial, steps, invariant)


MEMBERSHIP_CONTROLS = {"BUG_blind_membership_write": dict(versioned=False), "BUG_join_ignores_the_ceiling": dict(check_cap=False)}


def promotion(import_before_seal=False, descriptor_grants_authority=False, resolve_unknown=True,
              promoters=2, records=2):
    """Promote index X out of lane group S into its own domain T while lane-mate Y keeps writing.

    S is one ordered lane: its writer appends X writes, Y writes and a SEAL(X) marker, and refuses X
    writes after the marker (a delayed source proposal is fenced by order). The activation head of X
    moves S -> SEALED -> T by CAS on its (version, mode). T proves it can recover the sealed prefix
    (imports it) before the activation. Every CAS may report success, failure, or an unknown outcome in
    which the CAS did or did not apply; a promoter resolves an unknown outcome by reading the head and
    rolls forward, never back. Clients route by a descriptor that may be stale: a write may be sent
    to S or T whatever the head says, and the target decides. Not modelled: S's lane internals (the
    `lane` model), T's own lane, and any rollback path (a failed promotion stays SEALED, unavailable
    but never two authorities).
    Controls: import_before_seal=True, descriptor_grants_authority=True, resolve_unknown=False."""
    # (slog, xsealed, act(ver, mode), tbase, tlog, acked_x, acked_y, nextrec, promoters (stage, expected act))
    initial = ((), False, (0, "S"), None, (), frozenset(), frozenset(), 0, (("idle", None),) * promoters)

    def prefix(slog):
        out = set()
        for entry in slog:
            if entry[0] == "SEAL":
                break
            if entry[0] == "X":
                out.add(entry[1])
        return out

    def invariant(state):
        slog, xs, act, tbase, tlog, ax, ay, _, _ = state
        if act[1] == "T" and tbase is None:
            return "the activation names a target that never proved the sealed prefix"
        recovered = set(tbase or ()) | set(tlog) if act[1] == "T" else prefix(slog)
        if ax - recovered:
            return f"acknowledged X write(s) {sorted(ax - recovered)} are not recoverable from the authority the head names"
        if ay - {e[1] for e in slog if e[0] == "Y"}:
            return "a lane-mate's acknowledged write was lost"
        return None

    def steps(state):
        slog, xs, act, tbase, tlog, ax, ay, nextrec, ps = state
        if nextrec < records:
            r = nextrec
            if not xs or descriptor_grants_authority:
                yield f"X write {r} routed to S is acknowledged", (slog + (("X", r),), xs, act, tbase, tlog, ax | {r}, ay, nextrec + 1, ps)
            else:
                yield f"X write {r} routed to S is refused (sealed)", state
            if act[1] == "T":
                yield f"X write {r} routed to T is acknowledged", (slog, xs, act, tbase, tlog + (r,), ax | {r}, ay, nextrec + 1, ps)
            yield f"Y write {r} to S", (slog + (("Y", r),), xs, act, tbase, tlog, ax, ay | {r}, nextrec + 1, ps)
        for p, (stage, expected) in enumerate(ps):
            def me(new_stage, new_expected=None):
                return put(ps, p, (new_stage, new_expected))
            if stage == "idle":
                yield f"promoter {p} reads the activation head ({act[1]})", state[:8] + (me("read", act),)
            elif stage == "read":
                if expected[1] == "T":
                    yield f"promoter {p} finds X already in T", state[:8] + (me("done"),)
                elif expected[1] == "SEALED":
                    yield f"promoter {p} finds X already sealed", state[:8] + (me("sealed", expected),)
                else:
                    if not xs:
                        yield f"promoter {p} appends SEAL(X) to S", (slog + (("SEAL",),), True) + state[2:8] + (me("marked", expected),)
                    else:
                        yield f"promoter {p} finds SEAL(X) already in S", state[:8] + (me("marked", expected),)
                    if import_before_seal and tbase is None:
                        yield (f"promoter {p} imports X's records before sealing", state[:3] + (frozenset(e[1] for e in slog if e[0] == "X"),) + state[4:8] + (me("read", expected),))
            elif stage == "marked":
                if act == expected:
                    new = (act[0] + 1, "SEALED")
                    yield f"promoter {p} CAS S->SEALED applies, reply delivered", state[:2] + (new,) + state[3:8] + (me("sealed", new),)
                    yield f"promoter {p} CAS S->SEALED applies, reply lost", state[:2] + (new,) + state[3:8] + (me("unknown"),)
                    yield f"promoter {p} CAS S->SEALED lost in transit", state[:8] + (me("unknown"),)
                else:
                    yield f"promoter {p} CAS S->SEALED fails", state[:8] + (me("unknown"),)
            elif stage == "sealed":
                if tbase is None:
                    yield f"promoter {p} imports the sealed prefix into T", state[:3] + (frozenset(prefix(slog)),) + state[4:8] + (me("imported", expected),)
                else:
                    yield f"promoter {p} finds T already holds a prefix", state[:8] + (me("imported", expected),)
            elif stage == "imported":
                if act == expected:
                    new = (act[0] + 1, "T")
                    yield f"promoter {p} CAS SEALED->T applies, reply delivered", state[:2] + (new,) + state[3:8] + (me("done"),)
                    yield f"promoter {p} CAS SEALED->T applies, reply lost", state[:2] + (new,) + state[3:8] + (me("unknown"),)
                    yield f"promoter {p} CAS SEALED->T lost in transit", state[:8] + (me("unknown"),)
                else:
                    yield f"promoter {p} CAS SEALED->T fails", state[:8] + (me("unknown"),)
            elif stage == "unknown":
                if resolve_unknown:
                    yield f"promoter {p} reads the head ({act[1]}) to resolve an unknown outcome", state[:8] + (me("read", act),)
                else:
                    yield (f"promoter {p} treats the unknown outcome as failure and reopens S",
                           (slog, False, (act[0] + 1, "S")) + state[3:8] + (me("done"),))
    return explore(initial, steps, invariant)


PROMOTION_CONTROLS = {
    "BUG_import_before_the_seal": dict(import_before_seal=True),
    "BUG_descriptor_grants_write_authority": dict(descriptor_grants_authority=True),
    "BUG_rollback_over_a_seal_after_an_unknown_outcome": dict(resolve_unknown=False),
}


# --------------------------------------------------------------------------------------------------
# keys: lane-mate revocation histories plus AAD/replay, rotation, reinstatement
# --------------------------------------------------------------------------------------------------
KEY_OPS = ("append_a", "append_b", "recover", "revoke_a", "revoke_b", "reinstate_a", "rotate_a", "replay_a_as_b", "replay_a_again", "replay_a_in_place_as_b")


def key_histories(sharing, recovery="skip", aad="index_and_position", revoke_scope="all_versions", length=6):
    """Exhaustive histories of `length` operations ending in recovery, over one lane shared by indexes a, b.

      sharing   per_index_dek | lane_dek_default | lane_dek_a_byok (BROKEN CONTROL: a revocable key wraps
                the shared lane's DEK, so b's blocks are under key_a)
      recovery  skip (blocks of a revoked key are skipped and kept) | fail (the lane fails) |
                drop (recovery rewrites the lane without them, so a reinstated key finds nothing)
      aad       index_and_position | index_only | position_only | none: what the AEAD authenticates besides the key
      revoke_scope  all_versions | current_version (BROKEN CONTROL: rotation leaves old versions readable)

    A key is revoked as a whole, whatever its versions: after `revoke_x` every block under it is
    unreadable, after `reinstate_a` readable again. Invariant at every recovery: the lane recovers;
    a record is applied exactly when its index's own key is not revoked now; and nothing is applied
    that was not appended there (no replay across indexes or positions). A slot position is never
    reused. Limits: cryptography is symbolic, the adversary can only copy whole blocks, and a block
    failing authentication is skipped and not applied (an availability choice for M05 to confirm)."""
    own = {"per_index_dek": {"a": "key_a", "b": "key_b"}, "lane_dek_default": {"a": "default", "b": "default"},
           "lane_dek_a_byok": {"a": "key_a", "b": "key_b"}}[sharing]
    protect = {"per_index_dek": own, "lane_dek_default": own, "lane_dek_a_byok": {"a": "key_a", "b": "key_a"}}[sharing]
    revoke_key = {"revoke_a": own["a"], "revoke_b": own["b"]}
    ops = KEY_OPS if sharing != "lane_dek_default" else tuple(o for o in KEY_OPS if o != "revoke_b")
    checked = 0

    def fail(why, history):
        return {"safe": False, "histories": checked, "counterexample": list(history), "why": why}

    for history in itertools.product(ops, repeat=length):
        if history[-1] != "recover":
            continue
        checked += 1
        lane, appended, revoked_keys, revoked_versions, versions, seq = [], [], set(), {}, {k: 1 for k in set(own.values())}, 0
        for op in history:
            if op.startswith("append_"):
                idx = op[-1]
                lane.append({"index": idx, "origin": (idx, seq), "pos": seq, "kver": versions[protect[idx]], "aead_index": idx, "aead_pos": seq})
                appended.append((idx, seq))
                seq += 1
            elif op in revoke_key:
                key = revoke_key[op]
                revoked_keys.add(key)
                revoked_versions.setdefault(key, set()).update(range(1, 99) if revoke_scope == "all_versions" else {versions[key]})
            elif op == "reinstate_a":
                revoked_keys.discard(own["a"])
                revoked_versions.pop(own["a"], None)
            elif op == "rotate_a":
                versions[protect["a"]] += 1
            elif op in ("replay_a_as_b", "replay_a_again", "replay_a_in_place_as_b"):
                src = next((r for r in lane if r["origin"][0] == "a" and not r.get("replay")), None)
                if src is not None:
                    copy = dict(src, pos=src["pos"] if op == "replay_a_in_place_as_b" else seq, replay=True)
                    if op != "replay_a_again":
                        copy["index"] = "b"
                    lane.append(copy)
                    seq += 1
            else:  # recover
                applied, keep = [], []
                for r in lane:
                    unreadable = r["kver"] in revoked_versions.get(protect[r["aead_index"]], ())
                    if recovery == "fail" and unreadable:
                        return fail("recovery failed the whole lane on a block whose key is revoked", history)
                    if recovery == "drop" and unreadable:
                        continue
                    keep.append(r)
                    index_ok = r["index"] == r["aead_index"]
                    pos_ok = r["pos"] == r["aead_pos"]
                    authentic = {"none": True, "index_only": index_ok, "position_only": pos_ok}.get(aad, index_ok and pos_ok)
                    if not unreadable and authentic:
                        applied.append(r)
                lane = keep
                for r in applied:
                    if r["index"] != r["origin"][0]:
                        return fail(f"a block of index {r['origin'][0]} was applied as index {r['index']}", history)
                origins = [r["origin"] for r in applied]
                if len(set(origins)) != len(origins):
                    return fail("a replayed block was applied twice", history)
                expected = {o for o in appended if own[o[0]] not in revoked_keys}
                got = set(origins)
                if got != expected:
                    r = sorted(got ^ expected)[0]
                    return fail(f"record {r} readable={r in got} but its own key is {'not ' if r in expected else ''}revoked", history)
    return {"safe": True, "histories": checked}


KEY_CASES = {
    "per_index_dek, skip, authenticated index and position": dict(sharing="per_index_dek"),
    "lane_dek_default, skip": dict(sharing="lane_dek_default"),
    "BUG_revocable_key_wraps_a_shared_lane": dict(sharing="lane_dek_a_byok"),
    "BUG_recovery_fails_the_lane": dict(sharing="per_index_dek", recovery="fail"),
    "BUG_recovery_drops_revoked_blocks": dict(sharing="per_index_dek", recovery="drop"),
    "BUG_no_aad": dict(sharing="per_index_dek", aad="none"),
    "BUG_aad_without_the_index": dict(sharing="per_index_dek", aad="position_only"),
    "BUG_aad_without_the_position": dict(sharing="per_index_dek", aad="index_only"),
    "BUG_revoke_only_the_current_key_version": dict(sharing="per_index_dek", revoke_scope="current_version"),
}


def lane_mate_histories_agree():
    """The M03.09 stand-in search and this model must agree on its base histories."""
    ref = {s: lane_model.revocation_search(s) for s in ("per_index_dek", "lane_dek_default", "lane_dek_a_byok")}
    ref_fail = lane_model.revocation_search("per_index_dek", recovery="fail")
    return ref, ref_fail


# --------------------------------------------------------------------------------------------------
def run_all(wide=False):
    """`wide` adds the 3-instance checkpoint configuration (about 2.8M states, about a minute)."""
    out = {"bounds": {"lane": LANE_BOUNDS}, "models": {}}
    m = out["models"]
    m["lane_authority"] = {"correct": lane_authority()}
    for name, kwargs in LANE_CONTROLS.items():
        m["lane_authority"][name] = lane_authority(**kwargs)
    m["lane_checkpoint_and_read"] = {"correct": lane_authority(checkpoint=True, reads=True, instances=2, slots=4, records=3)}
    for name, kwargs in CHECKPOINT_CONTROLS.items():
        m["lane_checkpoint_and_read"][name] = lane_authority(checkpoint=True, reads=True, instances=2, slots=4, records=3, **kwargs)
    for name, kwargs in READ_CONTROLS.items():
        m["lane_checkpoint_and_read"][name] = lane_authority(checkpoint=True, reads=True, instances=2, slots=4, records=3, **kwargs)
    if wide:
        m["lane_checkpoint_and_read"]["correct, three instances, one checkpoint"] = lane_authority(
            checkpoint=True, reads=True, instances=3, slots=4, records=3, checkpoints=1)
    m["pin_gc"] = {"correct": pin_gc()}
    for name, kwargs in PIN_CONTROLS.items():
        m["pin_gc"][name] = pin_gc(**kwargs)
    m["membership"] = {"correct": membership()}
    for name, kwargs in MEMBERSHIP_CONTROLS.items():
        m["membership"][name] = membership(**kwargs)
    m["promotion"] = {"correct": promotion()}
    for name, kwargs in PROMOTION_CONTROLS.items():
        m["promotion"][name] = promotion(**kwargs)
    m["keys"] = {name: key_histories(**kwargs) for name, kwargs in KEY_CASES.items()}
    return out


def verdict_ok(name, result):
    return result["safe"] == (not name.startswith("BUG"))


def markdown(result):
    lines = []
    for model, cases in result["models"].items():
        lines += [f"### {model}", "", "| case | verdict | states / histories | counterexample (shortest) |", "|---|---|---|---|"]
        for name, r in cases.items():
            size = r.get("states", r.get("histories"))
            if r["safe"]:
                lines.append(f"| {name} | safe | {size:,} | |")
            else:
                trace = r.get("counterexample") or []
                why = r.get("violation") or r.get("why")
                lines.append(f"| {name} | **unsafe** (expected) | {size:,} | {' → '.join(trace)} — {why} |")
        lines.append("")
    return "\n".join(lines)


def selftest():
    result = run_all()
    for model, cases in result["models"].items():
        for name, r in cases.items():
            assert verdict_ok(name, r), (model, name, r)
    ref, ref_fail = lane_mate_histories_agree()
    assert ref["per_index_dek"]["safe"] and ref["lane_dek_default"]["safe"] and not ref["lane_dek_a_byok"]["safe"] and not ref_fail["safe"]
    print("PASS: every correct model is safe and every seeded control is refused")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--markdown", action="store_true")
    parser.add_argument("--wide", action="store_true", help="also run the 3-instance checkpoint configuration")
    args = parser.parse_args()
    if args.selftest:
        selftest()
        return
    result = run_all(wide=args.wide)
    print(json.dumps(result, indent=2) if args.json else markdown(result))


if __name__ == "__main__":
    main()
