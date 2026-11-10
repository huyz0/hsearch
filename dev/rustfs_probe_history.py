"""Predeclared matrix and bounded version-two capture framing.

Provider tokens refer to producing operations. They are never guessed, ordered,
truncated or treated as payload hashes. Plan construction precedes all I/O.
"""
from __future__ import annotations

import hashlib
import json
import re
import threading
import time

CLIENTS, OPERATIONS, ATTEMPTS, ATOMS = 16, 256, 512, 1024
WIRE_BYTES, DIAGNOSTIC_BYTES = 704 * 1024, 64 * 1024
TOTAL_BYTES = WIRE_BYTES + DIAGNOSTIC_BYTES
FIELDS = ("id", "scenario", "client", "action", "key", "parent", "seed", "length", "offset", "range_length", "race", "fault")
ACTIONS = ("create", "update", "resolve", "range", "head", "restart", "init", "part", "complete", "abort", "parts", "uploads", "bucket", "versioning", "versions", "resolve-head", "replay", "stale-update", "reuse-id", "complete-invalid", "race-head", "race-range")


def need(condition, code):
    if not condition:
        raise ValueError(code)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def workload(seed=1729):
    need(type(seed) is int and 0 <= seed < 2**32, "seed")
    rows = []

    def add(scenario, action, key, *, parent=0, client=0, length=0, offset=0,
            range_length=0, race=0, fault="none", body_seed=None):
        row = dict(zip(FIELDS, (len(rows) + 1, scenario, client, action, key, parent,
                               seed + len(rows) if body_seed is None else body_seed,
                               length, offset, range_length, race, fault)))
        rows.append(row)
        return row["id"]

    add(0, "bucket", "")
    add(0, "versioning", "")

    def resolution(scenario, key, original):
        listing = add(scenario, "versions", key, parent=original)
        for candidate in range(16):
            add(scenario, "resolve-head", key, parent=listing, offset=candidate)
        add(scenario, "resolve", key, parent=original)

    for client in range(CLIENTS):
        add(0, "create", "create-race", client=client, length=1024, race=1)
    add(0, "race-head", "create-race", offset=1)
    add(0, "race-range", "create-race", offset=1, range_length=1024)
    initial = add(1, "create", "update-race", length=2048)
    for client in range(CLIENTS):
        add(1, "update", "update-race", parent=initial, client=client, length=2048, race=2)
    add(1, "race-head", "update-race", offset=2)
    add(1, "race-range", "update-race", offset=2, range_length=2048)
    lost = add(2, "create", "suppressed", length=4096, fault="suppress")
    add(2, "reuse-id", "different-key", parent=lost, length=4096)
    latest = add(2, "head", "suppressed", parent=lost)
    add(2, "update", "suppressed", parent=latest, length=4096)
    resolution(2, "suppressed", lost)
    unknown = add(2, "create", "socket-loss", length=4096, fault="disconnect")
    resolution(2, "socket-loss", unknown)
    first = add(3, "create", "range-aba", length=16384, body_seed=seed)
    add(3, "range", "range-aba", parent=first, offset=255, range_length=257)
    second = add(3, "update", "range-aba", parent=first, length=16384, body_seed=seed+1)
    add(3, "range", "range-aba", parent=first, offset=255, range_length=257)
    third = add(3, "update", "range-aba", parent=second, length=16384, body_seed=seed)
    add(3, "range", "range-aba", parent=first, offset=255, range_length=257)
    add(3, "range", "range-aba", parent=third, offset=0, range_length=16384)
    add(3, "range", "range-aba", parent=third, offset=16384, range_length=1)
    replayed = add(3, "replay", "range-aba", parent=second, length=rows[second-1]["length"], body_seed=rows[second-1]["seed"])
    resolution(3, "range-aba", second)
    latest = add(3, "head", "range-aba", parent=replayed)
    add(3, "update", "range-aba", parent=latest, length=16384, body_seed=seed)
    add(3, "stale-update", "range-aba", parent=first, length=16384, body_seed=seed+99)
    saved = add(4, "create", "restart", length=8192)
    add(4, "restart", "restart", parent=saved)
    add(4, "head", "restart", parent=saved)
    add(4, "range", "restart", parent=saved, range_length=8192)
    upload = add(5, "init", "multipart")
    add(5, "part", "multipart", parent=upload, offset=1, length=5*1024**2)
    add(5, "part", "multipart", parent=upload, offset=2, length=4096)
    add(5, "part", "multipart", parent=upload, offset=2, length=4096)  # replacement
    add(5, "parts", "multipart", parent=upload)
    add(5, "complete-invalid", "multipart", parent=upload, offset=1)
    add(5, "complete-invalid", "multipart", parent=upload, offset=2)
    complete = add(5, "complete", "multipart", parent=upload)
    add(5, "range", "multipart", parent=complete, offset=5*1024**2-16, range_length=32)
    add(5, "range", "multipart", parent=complete, range_length=5*1024**2+4096)
    doomed = add(5, "init", "aborted")
    add(5, "part", "aborted", parent=doomed, offset=1, length=5*1024**2)
    add(5, "abort", "aborted", parent=doomed)
    add(5, "parts", "aborted", parent=doomed)
    lost_upload = add(5, "init", "lost-completion")
    add(5, "part", "lost-completion", parent=lost_upload, offset=1, length=5*1024**2)
    lost_complete = add(5, "complete", "lost-completion", parent=lost_upload, fault="suppress")
    resolution(5, "lost-completion", lost_complete)
    orphan = add(5, "init", "orphan")
    add(5, "part", "orphan", parent=orphan, offset=1, length=5*1024**2)
    add(5, "uploads", "orphan", parent=orphan)
    add(5, "abort", "orphan", parent=orphan)
    add(5, "uploads", "orphan", parent=orphan)
    need(len(rows) <= OPERATIONS, "operation-bound")
    return rows


def plan(binding, *, seed=1729):
    need(type(binding) is dict and set(binding) == {"digests", "run", "platform", "bucket"}, "binding-shape")
    need(type(binding["digests"]) is list and len(binding["digests"]) == 7 and
         all(re.fullmatch(r"[0-9a-f]{64}", x) for x in binding["digests"]), "digest-binding")
    need(re.fullmatch(r"[0-9a-f]{32}", binding["run"]) and type(binding["platform"]) is int and binding["platform"] in (1, 2) and
         re.fullmatch(r"[a-z0-9][a-z0-9-]{2,62}", binding["bucket"]), "run-binding")
    value = {"version": 2, "binding": binding, "seed": seed, "operations": workload(seed),
             "limits": {"clients": CLIENTS, "operations": OPERATIONS, "attempts": ATTEMPTS,
                        "atoms": ATOMS, "wire_bytes": WIRE_BYTES, "diagnostics": DIAGNOSTIC_BYTES,
                        "seconds": 120, "transitions": 100000, "working_bytes": 64*1024**2}}
    return value


def validate_plan(value):
    expected = plan(value["binding"], seed=value["seed"])
    need(value == expected, "plan-not-fixed-matrix")
    return value


def plan_wire(value):
    validate_plan(value)
    b = value["binding"]
    rows = ["HPPLAN02", "B\t" + "\t".join([*b["digests"], b["run"], str(b["platform"]), b["bucket"].encode().hex()])]
    for row in value["operations"]:
        rows.append("S\t" + "\t".join(str(row[k]) if k != "key" else row[k].encode().hex() for k in FIELDS))
    rows.append("END")
    raw = ("\n".join(rows) + "\n").encode()
    need(len(raw) <= 64*1024, "plan-wire-bound")
    return raw


class Recorder:
    """Serialized stamps and per-attempt budget reservation before any send."""

    def __init__(self, value, *, clock=time.monotonic, clock_origin=None, emit=None):
        self.plan = validate_plan(value)
        self.clock, self.started = clock, clock() if clock_origin is None else clock_origin
        need(0 <= clock()-self.started <= 120, "capture-origin")
        self.lock = threading.Lock()
        self.events, self.attempts, self.order, self.reserved, self.atoms = [], 0, 0, 0, set()
        self.intents, self.emit, self.emitted = [], emit, 0

    def _emit(self, line):
        raw = (line+"\n").encode()
        need(self.emitted+len(raw) <= 384*1024, "record-pool-exhausted")
        self.emitted += len(raw)
        if self.emit is not None:
            self.emit(raw)

    def stamp(self):
        micros = int((self.clock() - self.started)*1000000)
        need(0 <= micros <= 120000000, "history-deadline")
        return micros

    def reserve(self, spec):
        with self.lock:
            need(self.plan["operations"][spec["id"]-1] == spec, "unplanned-operation")
            need(self.attempts < ATTEMPTS and self.reserved + (16384 if spec["action"] == "versions" else 4096) <= WIRE_BYTES, "history-reservation")
            self.attempts += 1
            self.reserved += 16384 if spec["action"] == "versions" else 4096
            self.intents.append((spec["id"], self.attempts))
            # Persist an intent before any send. A crash between intent and
            # completion retains an explicitly unknown attempt, never rollback.
            self._emit("A\t"+str(spec["id"])+"\t"+str(self.attempts))
            return self.attempts

    def record(self, spec, attempt, result, *, request, condition="", selected="", auxiliary="", captured_times=None):
        # Foreign bodies/errors are never serialized. Only admitted opaque values
        # and stable codes enter the artifact, after exact field admission.
        with self.lock:
            need(not any(e["id"] == spec["id"] for e in self.events), "duplicate-operation")
            fields = [result.get(k, "") for k in ("etag", "version", "metadata")]
            fields += [request, condition, selected, auxiliary, result.get("error", "")]
            need(all(type(x) is str and len(x.encode()) <= (8192 if i == 6 else 1024) for i, x in enumerate(fields)), "event-field-bound")
            self.atoms.update(x for x in fields if x)
            need(len(self.atoms) <= ATOMS and sum(len(x.encode())+3 for x in self.atoms) <= 128*1024,
                 "dictionary-bound")
            need(result["boundary"] in {"not-transmitted", "during-transmit", "after-transmit-before-reply",
                                         "reply", "reply-suppressed", "not-applicable", "unimplemented", "host-restart"}, "boundary")
            start = result.get("start")
            end = result.get("end")
            start = self.stamp() if start is None else int((start-self.started)*1000000)
            end = self.stamp() if end is None else int((end-self.started)*1000000)
            if captured_times is not None:
                start, end = captured_times
            need(0 <= start <= end <= 120000000, "attempt-time")
            need(result.get("head_length") is None or type(result["head_length"]) is int and
                 0 <= result["head_length"] <= 16*1024**2, "head-length-bound")
            event = {"id": spec["id"], "attempt": attempt, "start": start, "end": end,
                     "status": result["status"], "boundary": result["boundary"],
                     "sent": result["request_bytes"], "received": result["response_bytes"],
                     "wire_received": result["response_wire_bytes"],
                     "head_length": result.get("head_length"),
                     "etag": fields[0], "version": fields[1], "metadata": fields[2], "request": request,
                     "condition": condition, "selected": selected, "auxiliary": auxiliary,
                     "digest": result["digest"], "error": result.get("error", "")}
            self.events.append(event)
            need(len(self.events) <= OPERATIONS, "event-count")
            self._emit(self.event_line(event))
            return event

    @staticmethod
    def event_line(e):
        values = [e[k] for k in ("id", "attempt", "start", "end", "status", "boundary", "sent", "received", "wire_received")]
        values += [e[k].encode().hex() for k in ("etag", "version", "metadata", "request", "condition", "selected", "auxiliary")]
        values += [e["digest"], e["error"].encode().hex(), "-" if e["head_length"] is None else str(e["head_length"])]
        return "E\t" + "\t".join(map(str, values))

    def wire(self, *, complete):
        prefix = hashlib.sha256(plan_wire(self.plan)).hexdigest()
        rows = ["HPCAP002", "P\t" + prefix]
        rows += ["A\t"+str(ident)+"\t"+str(attempt) for ident, attempt in self.intents]
        for e in sorted(self.events, key=lambda e: e["id"]):
            rows.append(self.event_line(e))
        rows.append("END\t" + str(int(complete)) + "\t" + str(self.attempts))
        raw = ("\n".join(rows)+"\n").encode()
        need(len(raw) <= WIRE_BYTES and self.emitted <= 384*1024,
             "history-wire-bound")
        return raw

    def restore(self, raw):
        """Only host-authenticated prior phase snapshots may be supplied here."""
        need(type(raw) is bytes and len(raw) <= WIRE_BYTES and not self.events, "resume-bound")
        lines = raw.decode("ascii").splitlines()
        need(lines[:2] == ["HPCAP002", "P\t"+hashlib.sha256(plan_wire(self.plan)).hexdigest()], "resume-plan")
        end = lines[-1].split("\t")
        need(len(end) == 3 and end[:2] == ["END", "0"], "resume-terminal")
        intent_lines = [line.split("\t") for line in lines[2:-1] if line.startswith("A\t")]
        need(all(len(parts) == 3 for parts in intent_lines), "resume-intent-frame")
        intended = [(int(parts[1]), int(parts[2])) for parts in intent_lines]
        need(len(intended) == int(end[2]) and len(set(intended)) == len(intended), "resume-intent-count")
        event_lines = [line.split("\t") for line in lines[2:-1] if not line.startswith("A\t")]
        need(all(len(parts) == 20 and parts[0] == "E" for parts in event_lines), "resume-frame")
        for parts in sorted(event_lines, key=lambda parts: int(parts[2])):
            need(len(parts) == 20 and parts[0] == "E", "resume-frame")
            integer = [int(x) for x in parts[1:6]]
            need(0 <= integer[2] <= integer[3] <= 120000000, "resume-time")
            strings = [bytes.fromhex(x).decode() for x in parts[10:17]]
            result = dict(status=integer[4], boundary=parts[6], request_bytes=int(parts[7]),
                          response_bytes=int(parts[8]), etag=strings[0], version=strings[1], metadata=strings[2],
                          response_wire_bytes=int(parts[9]), digest=parts[17], error=bytes.fromhex(parts[18]).decode(),
                          head_length=None if parts[19] == "-" else int(parts[19]),
                          start=self.started+integer[2]/1000000, end=self.started+integer[3]/1000000)
            spec = self.plan["operations"][integer[0]-1]
            attempt = self.reserve(spec)
            need(attempt == integer[1], "resume-attempts")
            need((spec["id"], attempt) in intended, "resume-intent-missing")
            # Preserve integer coordinates before encoding/charging a record;
            # floating-point monotonic seconds can lose a microsecond.
            self.record(spec, attempt, result, request=strings[3], condition=strings[4], selected=strings[5],
                        auxiliary=strings[6], captured_times=(integer[2], integer[3]))
        need(self.attempts == int(end[2]), "resume-count")
