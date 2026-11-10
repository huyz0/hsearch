#!/usr/bin/env python3
"""Finite public research schedules and independent result oracles, never runtime authority."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = Path("crates/testkit/tests/fixtures/durability-traces.json")
SEEDS = (7, 19, 43)
COUNTS = {"sparse": 16, "busy": 128, "skewed": 128}
OFFER_US = 500_000
PAYLOAD_BYTES = 1024
WAIT_US = (0, 10_000, 50_000, 250_000)
GROUPINGS = ("stream", "compatible")
MAX_REQUESTS = 4


def need(condition, message):
    if not condition:
        raise ValueError(message)


def integer(value, minimum, maximum, name):
    need(type(value) is int and minimum <= value <= maximum, "Invalid " + name)
    return value


def canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=True, separators=(",", ":")).encode("ascii")


def digest(value):
    return hashlib.sha256(canonical(value)).hexdigest()


def payload(event):
    prefix = ("id=" + event["id"] + ";tenant=" + str(event["tenant"]) +
              ";key=" + str(event["key"]) + ";stream=" + str(event["stream"]) + ";").encode("ascii")
    need(len(prefix) <= PAYLOAD_BYTES, "Payload identity exceeds bound")
    return prefix + b"#" * (PAYLOAD_BYTES - len(prefix))


def fixtures():
    traces = []
    for workload, count in COUNTS.items():
        for seed in SEEDS:
            state = seed
            def draw():
                nonlocal state
                state = (1664525 * state + 1013904223) & 0xffffffff
                return state
            permutation = list(range(count))
            for i in range(count - 1, 0, -1):
                j = draw() % (i + 1)
                permutation[i], permutation[j] = permutation[j], permutation[i]
            # Exactly floor(4*N/5) hot events, randomly interspersed; report the actual ratio.
            hot = set(permutation[:count * 4 // 5])
            events = []
            for i in range(count):
                stream = draw() % 16
                if workload == "skewed":
                    stream = draw() % 4 if i in hot else 4 + draw() % 12
                event = {"id": workload + "-" + str(seed) + "-" + str(i).zfill(3),
                         "at_us": i * OFFER_US // count, "stream": stream,
                         "tenant": stream // 4, "key": stream % 2}
                event["payload_sha256"] = hashlib.sha256(payload(event)).hexdigest()
                events.append(event)
            traces.append({"workload": workload, "seed": seed, "events": events})
    return {"schema_version": 1, "offer_window_us": OFFER_US, "payload_bytes": PAYLOAD_BYTES,
            "generator": "lcg32-fisher-yates-v1", "traces": traces}


def validate_trace(trace):
    need(type(trace) is dict and set(trace) == {"workload", "seed", "events"}, "Unknown trace fields")
    need(trace["workload"] in COUNTS, "Unknown workload")
    integer(trace["seed"], 0, 2**32 - 1, "seed")
    events = trace["events"]
    need(type(events) is list and 1 <= len(events) <= 128, "Trace record bound")
    seen, previous = set(), -1
    for event in events:
        need(type(event) is dict and set(event) == {"id", "at_us", "stream", "tenant", "key", "payload_sha256"},
             "Unknown event fields")
        need(type(event["id"]) is str and event["id"].isascii() and 1 <= len(event["id"]) <= 64,
             "Invalid public event identity")
        need(all(c.isalnum() or c == "-" for c in event["id"]), "Invalid event identity characters")
        need(event["id"] not in seen, "Duplicate offered identity")
        seen.add(event["id"])
        at = integer(event["at_us"], 0, OFFER_US - 1, "offer timestamp")
        need(at >= previous, "Reordered offered trace")
        previous = at
        stream = integer(event["stream"], 0, 15, "stream")
        need(integer(event["tenant"], 0, 3, "tenant") == stream // 4, "Tenant/stream mismatch")
        need(integer(event["key"], 0, 1, "key") == stream % 2, "Policy/stream mismatch")
        need(event["payload_sha256"] == hashlib.sha256(payload(event)).hexdigest(), "Payload digest mismatch")
    return events


def lane(event, grouping):
    need(grouping in GROUPINGS, "Unknown grouping policy")
    return (event["stream"],) if grouping == "stream" else (event["tenant"], event["key"])


def batches(trace, grouping, wait_us, byte_cap=16_384):
    """Create an IO-independent offered schedule; actual executor queueing is measured separately."""
    events = validate_trace(trace)
    need(grouping in GROUPINGS, "Unknown grouping policy")
    integer(wait_us, 0, 250_000, "batch wait")
    integer(byte_cap, PAYLOAD_BYTES, 16_384, "batch byte cap")
    need(byte_cap % PAYLOAD_BYTES == 0, "Nonintegral record byte cap")
    pending, result = {}, []
    def emit(key, at, reason):
        queue = pending.pop(key)
        result.append({"lane": list(key), "emit_us": at, "trigger": reason,
                       "ids": [event["id"] for event in queue]})
    for event in events:
        at = event["at_us"]
        # A timer firing exactly at an arrival wins the tie.
        due = sorted((queue[0]["at_us"] + wait_us, key) for key, queue in pending.items()
                     if queue[0]["at_us"] + wait_us <= at)
        for deadline, key in due:
            emit(key, deadline, "timer")
        key = lane(event, grouping)
        pending.setdefault(key, []).append(event)
        if len(pending[key]) * PAYLOAD_BYTES == byte_cap:
            emit(key, at, "bytes")
        elif wait_us == 0:
            emit(key, at, "timer")
    # The offered interval ending does not force an early flush.
    for deadline, key in sorted((queue[0]["at_us"] + wait_us, key) for key, queue in pending.items()):
        emit(key, deadline, "timer")
    ordinals = {event["id"]: i for i, event in enumerate(events)}
    result.sort(key=lambda row: (row["emit_us"], row["lane"], ordinals[row["ids"][0]]))
    validate_batches(trace, grouping, wait_us, byte_cap, result)
    return result


def validate_batches(trace, grouping, wait_us, byte_cap, rows):
    """Recompute conservation, compatibility and natural deadlines from an independent event ledger."""
    original = {e["id"]: e for e in validate_trace(trace)}
    ordinals = {identity: i for i, identity in enumerate(original)}
    need(grouping in GROUPINGS, "Unknown grouping policy")
    integer(wait_us, 0, 250_000, "batch wait")
    integer(byte_cap, PAYLOAD_BYTES, 16_384, "batch byte cap")
    need(byte_cap % PAYLOAD_BYTES == 0, "Nonintegral record byte cap")
    need(type(rows) is list and len(rows) <= len(original), "Batch row bound")
    observed = []
    for row in rows:
        need(type(row) is dict and set(row) == {"lane", "emit_us", "trigger", "ids"}, "Unknown batch fields")
        ids = row["ids"]
        need(type(ids) is list and 1 <= len(ids) <= byte_cap // PAYLOAD_BYTES, "Batch byte bound")
        need(all(type(i) is str and i in original for i in ids), "Unknown offered identity")
        events = [original[i] for i in ids]
        need(row["lane"] == list(lane(events[0], grouping)), "Batch lane identity mismatch")
        need(all(lane(e, grouping) == tuple(row["lane"]) for e in events), "Incompatible policy batch")
        times = [e["at_us"] for e in events]
        at = integer(row["emit_us"], 0, OFFER_US + 250_000, "planned emission")
        need(times == sorted(times) and max(times) <= at, "Future/reordered event emitted")
        first = min(times)
        if row["trigger"] == "timer":
            need(at == first + wait_us, "Early or late planned timer")
            need(all(t < at or wait_us == 0 and t == at for t in times), "Arrival/timer tie mismatch")
        elif row["trigger"] == "bytes":
            need(len(ids) * PAYLOAD_BYTES == byte_cap and at == max(times), "Invalid byte trigger")
            need(at < first + wait_us or wait_us == 0, "Byte flush bypassed due timer")
        else:
            raise ValueError("Unknown planned flush trigger")
        observed.extend(ids)
    need(len(observed) == len(original) and len(set(observed)) == len(original),
         "Dropped or duplicated offered event")
    # A second FIFO view rules out invented overlapping timers or omitting a
    # pending event from a timer batch while it is still below the byte cap.
    lane_events = {}
    for event in original.values():
        lane_events.setdefault(lane(event, grouping), []).append(event)
    for key, events in lane_events.items():
        consumed = 0
        selected = [r for r in rows if tuple(r["lane"]) == key]
        for row in sorted(selected, key=lambda r: (r["emit_us"], ordinals[r["ids"][0]])):
            end = consumed + len(row["ids"])
            need(row["ids"] == [e["id"] for e in events[consumed:end]], "Non-FIFO lane history")
            if row["trigger"] == "timer":
                need(wait_us != 0 or len(row["ids"]) == 1, "Zero wait must flush each arrival")
                need(end == len(events) or events[end]["at_us"] >= row["emit_us"],
                     "Timer omitted an already pending event")
            consumed = end
    return True


def validate_cas_rounds(rounds, initial_etag):
    """Eight sequential rounds, four distinct writes each; no predicted RustFS result."""
    need(type(rounds) is list and len(rounds) == 8, "CAS round matrix mismatch")
    need(type(initial_etag) is str and 1 <= len(initial_etag) <= 256, "Invalid initial ETag")
    previous_tag, previous_end = initial_etag, -1
    observed_peak, seen_tags = 0, {initial_etag}
    successes = conflicts = 0
    for number, row in enumerate(rounds):
        need(type(row) is dict and set(row) == {"round", "read_etag", "read_finished_ns", "barrier_ready_ns", "barrier_release_ns", "attempts", "resolution"},
             "Unknown CAS round fields")
        need(type(row["round"]) is int and row["round"] == number, "CAS round lineage mismatch")
        need(row["read_etag"] == previous_tag, "Stale per-round ETag lineage")
        read_end = integer(row["read_finished_ns"], 0, 2**63 - 1, "CAS read timestamp")
        need(read_end > previous_end, "Overlapping CAS rounds")
        attempts = row["attempts"]
        need(type(attempts) is list and len(attempts) == 4, "CAS contender matrix mismatch")
        need(all(type(x) is dict for x in attempts), "Invalid CAS attempt")
        need({x.get("contender") for x in attempts} == {0, 1, 2, 3}, "CAS contender identity mismatch")
        ready = row["barrier_ready_ns"]
        need(type(ready) is list and len(ready) == 4, "CAS barrier admission mismatch")
        release = integer(row["barrier_release_ns"], 0, 2**63 - 1, "CAS barrier release")
        need(all(read_end <= integer(t, 0, 2**63 - 1, "CAS contender ready") <= release for t in ready),
             "CAS contender missed the read/barrier")
        timing = []
        winners, ends, bodies = [], [], []
        for attempt in attempts:
            need(type(attempt) is dict and set(attempt) == {"contender", "if_match", "body", "start_ns", "end_ns", "status", "error_code"},
                 "Unknown CAS attempt fields")
            contender = integer(attempt["contender"], 0, 3, "CAS contender")
            expected = {"round": number, "generation": number + 1, "contender": contender}
            need(attempt["body"] == expected, "CAS must change generation/contender body")
            bodies.append(canonical(expected))
            need(attempt["if_match"] == previous_tag, "CAS attempt did not use barrier ETag")
            start = integer(attempt["start_ns"], 0, 2**63 - 1, "CAS start")
            end = integer(attempt["end_ns"], 0, 2**63 - 1, "CAS end")
            need(release <= start < end, "CAS timing order mismatch")
            ends.append(end)
            timing.extend(((start, 1), (end, -1)))
            status = integer(attempt["status"], 100, 599, "CAS HTTP status")
            if status == 200:
                need(attempt["error_code"] is None, "Successful CAS has error code")
                winners.append(expected)
                successes += 1
            else:
                need((status, attempt["error_code"]) in {(412, "PreconditionFailed"), (409, "ConditionalRequestConflict")},
                     "Unexpected CAS error cannot become a known conflict")
                conflicts += 1
        need(len(set(bodies)) == 4 and len(winners) == 1, "CAS did not resolve exactly one distinct winner")
        resolution = row["resolution"]
        need(type(resolution) is dict and set(resolution) == {"etag", "body", "finished_ns"},
             "Unknown CAS resolution fields")
        need(resolution["body"] == winners[0], "Resolved root does not match successful contender")
        tag = resolution["etag"]
        need(type(tag) is str and 1 <= len(tag) <= 256 and tag not in seen_tags, "CAS content/ETag did not advance")
        seen_tags.add(tag)
        previous_end = integer(resolution["finished_ns"], 0, 2**63 - 1, "CAS resolution timestamp")
        need(previous_end >= max(ends), "Next-round root observed before contenders finished")
        previous_tag = tag
        active = 0
        for _, change in sorted(timing):
            active += change
            need(0 <= active <= MAX_REQUESTS, "Global CAS admission limit exceeded")
            observed_peak = max(observed_peak, active)
    need((successes, conflicts) == (8, 24), "CAS outcome denominator mismatch")
    return {"rounds": 8, "attempts": 32, "successes": successes, "conflicts": conflicts,
            "admitted_concurrent_write_limit": MAX_REQUESTS, "observed_peak_overlap": observed_peak,
            "scope": "Oracle validation only; actual IO/source receipts remain independently required"}


def render(root=ROOT):
    path = root / FIXTURE
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(json.dumps(fixtures(), indent=2, ensure_ascii=True).encode("ascii") + b"\n")


def check(root=ROOT):
    expected = fixtures()
    need(json.loads((root / FIXTURE).read_text(encoding="utf-8")) == expected, "Public research fixture is stale")
    cases = []
    for trace in expected["traces"]:
        for grouping in GROUPINGS:
            for wait in WAIT_US:
                rows = batches(trace, grouping, wait)
                cases.append({"trace": trace["workload"] + "-" + str(trace["seed"]),
                              "grouping": grouping, "wait_us": wait, "batches": len(rows),
                              "schedule_sha256": digest(rows)})
    need(len(cases) == 72, "Offered batching schedule matrix changed")
    return {"fixture_sha256": hashlib.sha256((root / FIXTURE).read_bytes()).hexdigest(),
            "traces": 9, "offered_events": sum(len(t["events"]) for t in expected["traces"]),
            "planned_schedules": 72, "object_comparison_cases": 144,
            "scope": "Public deterministic offered schedules/oracles only; no IO, latency, durability or resource measurement"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("render", "check"))
    args = parser.parse_args()
    try:
        if args.command == "render":
            render()
        print(json.dumps(check(), sort_keys=True))
        return 0
    except (OSError, ValueError, KeyError, TypeError) as error:
        print("FAIL: " + str(error))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
