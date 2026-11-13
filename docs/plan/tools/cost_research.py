#!/usr/bin/env python3
"""Reproducible planning experiments. No real storage, crypto or network I/O."""
from __future__ import annotations

import argparse
import heapq
import json
import random
from collections import Counter, deque
from dataclasses import dataclass, replace
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def trace(kind, seed, seconds=30):
    rng = random.Random(seed)
    streams, rate = (10000, 100) if kind == "sparse" else (1000, 5000)
    t = 0.0
    events = []
    while True:
        t += rng.expovariate(rate)
        if t >= seconds:
            break
        stream = rng.randrange(50) if kind == "skewed" and rng.random() < .8 else rng.randrange(streams)
        # 50 indexes/tenant, two distinct key-policy groups per tenant.
        events.append((t, stream, 1024))
    return events


def batch(events, policy, delay, cap=4 * 1024 * 1024):
    active, heap, counts, waits = {}, [], Counter(), []
    generation = 0
    size_flushes = 0

    def flush(domain, when):
        state = active.pop(domain)
        counts[domain] += 1
        waits.extend(when - t for t in state["times"])

    for now, stream, size in events:
        while heap and heap[0][0] <= now:
            due, version, domain = heapq.heappop(heap)
            if domain in active and active[domain]["generation"] == version:
                flush(domain, due)
        domain = stream if policy == "per-shard" else (stream // 50, stream % 2)
        if domain in active and active[domain]["bytes"] + size > cap:
            flush(domain, now)
            size_flushes += 1
        if domain not in active:
            generation += 1
            active[domain] = {"generation": generation, "bytes": 0, "times": []}
            heapq.heappush(heap, (now + delay, generation, domain))
        active[domain]["bytes"] += size
        active[domain]["times"].append(now)
        if active[domain]["bytes"] >= cap:
            flush(domain, now)
            size_flushes += 1
    # Drain pending timers, retaining their real wait rather than flushing early.
    while heap:
        due, version, domain = heapq.heappop(heap)
        if domain in active and active[domain]["generation"] == version:
            flush(domain, due)
    waits.sort()
    assert len(waits) == len(events)
    assert all(-1e-9 <= w <= delay + 1e-8 for w in waits)
    return {
        "records": len(events), "batches": sum(counts.values()),
        "activated_domains": len(counts), "size_triggered_flushes": size_flushes,
        "batch_wait_p95_ms": 1000 * waits[int(.95 * (len(waits)-1))] if waits else 0,
        # Two structural checkpoint PUTs every 64 batches. No idle/final forced checkpoint.
        "checkpoint_puts": 2 * sum(n // 64 for n in counts.values()),
    }


@dataclass(frozen=True)
class State:
    slots: tuple = ()
    next_record: int = 0
    pending: tuple = ()
    poisoned: bool = False
    sealed: int = -1
    acked: tuple = ()


def fence_model(pipeline=1, poison=True, delete_fence=False, limit=3):
    """Bounded atomic-slot race model, deliberately not the entire epoch/GC protocol."""
    initial = State()
    queue, seen = deque([(initial, [])]), {initial}
    terminal = 0
    while queue:
        state, history = queue.popleft()
        if state.sealed >= 0:
            recovered = {x for x in state.slots[:state.sealed] if isinstance(x, int)}
            if not set(state.acked) <= recovered:
                return {"safe": False, "states": len(seen), "counterexample": history}
        actions = []
        if not state.poisoned and len(state.pending) < pipeline and state.next_record < limit:
            n = state.next_record
            actions.append((replace(state, next_record=n+1, pending=state.pending+(n,)), f"issue record {n}"))
        for n in state.pending:
            slots = list(state.slots)
            slots.extend([None] * max(0, n + 1 - len(slots)))
            pending = tuple(x for x in state.pending if x != n)
            if slots[n] is None:
                slots[n] = n
                actions.append((replace(state, slots=tuple(slots), pending=pending, acked=state.acked+(n,)), f"PUT/ack record {n}"))
                # Same persisted slot, lost reply. Never call an unknown outcome a definite refusal.
                actions.append((replace(state, slots=tuple(slots), pending=pending, poisoned=poison), f"PUT {n} persisted, reply lost"))
            else:
                actions.append((replace(state, pending=pending, poisoned=poison), f"record {n} collides with occupied slot"))
        if state.sealed < 0:
            slots = list(state.slots)
            slot = next((i for i,x in enumerate(slots) if x is None), len(slots))
            if slot == len(slots):
                slots.append("FENCE")
            else:
                slots[slot] = "FENCE"
            actions.append((replace(state, slots=tuple(slots), sealed=slot), f"successor fences slot {slot}"))
        elif delete_fence and state.slots[state.sealed] == "FENCE":
            slots = list(state.slots)
            slots[state.sealed] = None
            actions.append((replace(state, slots=tuple(slots)), "BUG: GC deletes fence"))
        if not actions:
            terminal += 1
        for nxt, action in actions:
            if nxt not in seen:
                seen.add(nxt)
                queue.append((nxt, history+[action]))
    return {"safe": True, "states": len(seen), "terminal_states": terminal,
            "bounds": {"records": limit, "one_old_writer": True, "one_successor": True,
                       "atomic_create": True, "no_epoch_or_full_gc_model": True}}


def run():
    rows = []
    for workload in ("sparse", "busy", "skewed"):
        for seed in (7,19,43):
            events = trace(workload, seed)
            for policy in ("per-shard", "tenant-key"):
                for delay in (.05,.25,1.0,5.0):
                    row = batch(events, policy, delay)
                    row.update(workload=workload, seed=seed, policy=policy, max_wait_s=delay, arrival_window_s=30)
                    n, cp = row["batches"], row["checkpoint_puts"]
                    row["put_counts"] = {
                        "payload_plus_per_batch_CAS_inline_key": 2*n+cp,
                        "fenced_append_inline_key": n+cp,
                        "fenced_append_separate_key_object": 2*n+cp,
                    }
                    row["application_wrap_calls"] = n
                    row["structural_activation_puts_separate"] = 2 * row["activated_domains"]
                    rows.append(row)
    models = {"serialized_poisoned_retained": fence_model(),
              "BUG_two_inflight": fence_model(pipeline=2),
              "BUG_continue_after_failure": fence_model(poison=False),
              "BUG_deleted_fence": fence_model(delete_fence=True)}
    assert models["serialized_poisoned_retained"]["safe"]
    assert all(not result["safe"] for name,result in models.items() if name.startswith("BUG"))
    assert batch([], "tenant-key", 1)["batches"] == 0
    # Grouping must never merge different tenant/key policies.
    assert batch([(0,0,1),(0,1,1),(0,50,1)], "tenant-key", 1)["batches"] == 3
    return {"status": "planning simulation and bounded model only; NOT a storage/latency/crypto proof",
            "assumptions": {"duration_s":30,"record_encoded_bytes":1024,"cap_bytes":4194304,
                "seeds":[7,19,43],"domains":"50 indexes per tenant, two key policies; one ordered lane per domain",
                "checkpoint_every_batches":64,"checkpoint_puts":2,
                "cost_omissions":"GET/LIST/takeover/fence retention/retries/segments/rewrap/compute/billing; activation counts separate",
                "timing":"batch waiting only; excludes KMS, S3, queue/prepare/apply/refresh/network; fixed arrival trace drained through final timer"},
            "rows":rows,"fencing_model":models}


def summary(data):
    lines=["# Immediate cost research experiment results", "",
           "Generated by `python docs/plan/tools/cost_research.py --write`. These are seeded synthetic batching experiments and a bounded atomic-slot model, not hsearch runtime or cloud measurements.", "",
           "| Workload | Policy | Batch wait cap | Batches/s, mean of 3 seeds | Mean records/batch | P95 batch wait only |", "| --- | --- | --- | --- | --- | --- |"]
    for kind in ("sparse","busy","skewed"):
        for policy in ("per-shard","tenant-key"):
            for delay in (.05,.25,1.0,5.0):
                r=[x for x in data["rows"] if (x["workload"],x["policy"],x["max_wait_s"])==(kind,policy,delay)]
                batches=sum(x["batches"] for x in r)
                lines.append(f"| {kind} | {policy} | {delay:g} s | {batches/90:.2f} | {sum(x['records'] for x in r)/batches:.2f} | {sum(x['batch_wait_p95_ms'] for x in r)/3:.1f} ms |")
    lines += ["", "## Bounded fence model", ""]
    for name, result in data["fencing_model"].items():
        lines += [f"- {name}: safe={result['safe']}, states examined={result['states']}."]
        if "counterexample" in result:
            lines += ["  Counterexample: " + "; ".join(result["counterexample"]) + "."]
    lines += ["", "The correct model covers one old writer, one successor, three record identities, atomic conditional create, failure poisoning, and a retained fence. It excludes full activation/ownership, idempotency, actual provider retries/linearizability, multiple terms, checkpoint/GC, same-term reopen and crypto. Runtime implementation must prove those separately. Bad controls are intentionally unsafe.", "",
              "Each batch in the raw JSON counts payload+CAS, fenced+inline-envelope, and fenced+separate-envelope variants. KMS calls remain one per fresh batch; an inline wrapped key saves an object PUT, not a KMS request. Two checkpoint PUTs are charged every 64 batches, and per-domain activation is shown separately rather than multiplied into a steady-state monthly claim.", ""]
    return "\n".join(lines)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write",action="store_true")
    args=parser.parse_args()
    data=run()
    if args.write:
        (ROOT/"archive/evidence/cost-research-results.json").write_text(json.dumps(data,indent=2)+"\n",encoding="utf-8",newline="\n")
        (ROOT/"archive/evidence/cost-research-results.md").write_text(summary(data),encoding="utf-8",newline="\n")
    print(summary(data))


if __name__ == "__main__":
    main()
