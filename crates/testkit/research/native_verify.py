"""Independent bounded parsing/accounting of partial native adapter probe artifacts.

This verifies retained bytes and local probe ledgers. It does not accept the
M02.10 workload/target/profile/repetition/source/coverage/review matrix.
The protobuf field map is the exact pinned raft-rs eraftpb.proto schema.
"""
from __future__ import annotations

from collections import Counter
import hashlib
import json
from pathlib import Path
import re

CAP = 1048576
MESSAGE_NAMES = ("MsgHup", "MsgBeat", "MsgPropose", "MsgAppend", "MsgAppendResponse", "MsgRequestVote",
                 "MsgRequestVoteResponse", "MsgSnapshot", "MsgHeartbeat", "MsgHeartbeatResponse", "MsgUnreachable",
                 "MsgSnapStatus", "MsgCheckQuorum", "MsgTransferLeader", "MsgTimeoutNow", "MsgReadIndex",
                 "MsgReadIndexResp", "MsgRequestPreVote", "MsgRequestPreVoteResponse")


def need(value, reason):
    if not value:
        raise ValueError(reason)


def integer(value, low, high, label):
    need(type(value) is int and low <= value <= high, label + " integer bound")
    return value


def unique(pairs):
    result = {}
    for key, value in pairs:
        need(key not in result, "Duplicate native JSON field")
        result[key] = value
    return result


def decode(raw):
    need(type(raw) is bytes and len(raw) <= CAP, "Native JSON byte bound")
    level, quoted, escaped = 0, False, False
    for byte in raw:
        if quoted:
            if escaped:
                escaped = False
            elif byte == 92:
                escaped = True
            elif byte == 34:
                quoted = False
        elif byte == 34:
            quoted = True
        elif byte in (91, 123):
            level += 1
            need(level <= 32, "Native JSON nesting bound")
        elif byte in (93, 125):
            level -= 1
            need(level >= 0, "Native JSON framing")
    need(not quoted and level == 0, "Native JSON framing")
    def constant(_value):
        raise ValueError("Nonfinite native JSON value")
    return json.loads(raw.decode("utf-8", "strict"), object_pairs_hook=unique, parse_constant=constant)


def framed_state(raw):
    need(type(raw) is bytes and 16 <= len(raw) <= CAP and raw[:8] == b"HSRCHD01" and
         int.from_bytes(raw[8:16],"little") == len(raw)-16,"Native persisted witness framing")
    return decode(raw[16:])


def extent(row, files):
    location = row.get('wire')
    need(type(location) is dict and set(location) == {'file', 'offset', 'bytes'}, 'Transaction extent shape')
    name = location['file']
    need(type(name) is str and re.fullmatch(r'wire-[0-9]{3}\.bin', name), 'Transaction extent path')
    offset = integer(location['offset'], 0, CAP, 'transaction offset')
    size = integer(location['bytes'], 1, CAP, 'transaction extent')
    raw = files.get('artifacts/' + name)
    need(type(raw) is bytes and offset + size <= len(raw) <= CAP, 'Truncated transaction extent')
    return raw[offset:offset + size]


def content_delta(previous, delta):
    """Reconstruct exact bytes, never trust an entry/hash declaration as content."""
    need(type(previous) is bytes and len(previous) <= CAP and type(delta) is bytes and
         16 <= len(delta) <= CAP and delta[:8] == b'HSRCHW01', 'Transaction delta framing')
    old = int.from_bytes(delta[8:12], 'little')
    size = integer(int.from_bytes(delta[12:16], 'little'), 16, CAP, 'transaction image')
    need(old == len(previous), 'Transaction delta predecessor size')
    cursor, count, output = 16, 0, bytearray()
    while cursor < len(delta):
        tag = delta[cursor]; cursor += 1
        count += 1
        need(count <= 4096 and tag in (0, 1), 'Transaction delta record bound/type')
        width = 8 if tag == 0 else 4
        need(cursor + width <= len(delta), 'Truncated transaction delta record')
        if tag == 0:
            offset = int.from_bytes(delta[cursor:cursor + 4], 'little')
            length = int.from_bytes(delta[cursor + 4:cursor + 8], 'little')
            cursor += 8
            need(0 < length <= size - len(output) and offset + length <= len(previous), 'Transaction copy bounds')
            output.extend(previous[offset:offset + length])
        else:
            length = int.from_bytes(delta[cursor:cursor + 4], 'little'); cursor += 4
            need(0 < length <= size - len(output) and cursor + length <= len(delta), 'Transaction literal bounds')
            output.extend(delta[cursor:cursor + length]); cursor += length
    need(len(output) == size, 'Transaction delta incomplete image')
    return bytes(output)


class Transactions:
    """Bounded disk lifecycle and IO proof, independent of adapter callbacks."""
    IO = {'file_write_attempt', 'file_write', 'sync_attempt', 'sync_complete',
          'rename_complete', 'disk_persisted', 'disk_error'}

    def __init__(self, files):
        self.files, self.disks = files, {}

    def observe(self, row):
        kind = row['kind']
        if kind not in self.IO | {'disk_open', 'disk_reopened', 'disk_transaction'}:
            return None
        name = row.get('directory')
        need(type(name) is str and re.fullmatch(r'node-[1-4](-(log|machine))?', name), 'Transaction disk identity')
        if kind == 'disk_open':
            need(name not in self.disks and len(self.disks) < 12 and
                 type(row.get('incarnation')) is int and row['incarnation'] == 1 and
                 type(row.get('predecessor')) is int and row['predecessor'] == 0, 'Disk identity reused/unseeded')
            self.disks[name] = {'incarnation': 1, 'attempted': 0, 'completed': 0,
                                'raw': b'', 'pending': None, 'poisoned': False, 'token': None}
            return None
        need(name in self.disks, 'Transaction on unopened disk')
        disk = self.disks[name]
        if kind == 'disk_reopened':
            need(disk['pending'] is None and disk['raw'] and
                 integer(row['incarnation'], 2, 4096, 'disk incarnation') == disk['incarnation'] + 1 and
                 type(row['predecessor']) is int and row['predecessor'] == disk['completed'], 'Disk reopen lineage')
            raw = content_delta(disk['raw'], extent(row, self.files))
            need(raw == disk['raw'], 'Reopened bytes differ from last completed transaction')
            framed_state(raw)
            disk.update(incarnation=row['incarnation'], poisoned=False)
            return None
        token = row.get('transaction')
        need(type(token) is dict and set(token) == {'disk', 'incarnation', 'id', 'predecessor'} and
             token['disk'] == name and type(token['incarnation']) is int and
             token['incarnation'] == disk['incarnation'], 'Transaction disk/incarnation association')
        identity = integer(token['id'], 1, 65536, 'transaction id')
        integer(token['predecessor'], 0, 65536, 'transaction predecessor')
        if kind == 'disk_transaction':
            need(disk['pending'] is None and identity == disk['attempted'] + 1 and
                 token['predecessor'] == disk['completed'], 'Transaction reused/missing predecessor')
            disk['attempted'] = identity
            disk['pending'] = {'token': dict(token), 'bytes': 0, 'write': False,
                               'sync': None, 'file': False, 'rename': False, 'directory': False}
            return None
        pending = disk['pending']
        need(pending is not None and token == pending['token'], 'IO lacks exact active transaction')
        if kind == 'disk_error':
            need(row['poisoned_until_reload'] is True, 'Failed transaction not poisoned')
            disk.update(pending=None, poisoned=True)
            return None
        need(not disk['poisoned'], 'Poisoned disk wrote before reload')
        if kind == 'file_write_attempt':
            need(not pending['write'] and not pending['file'] and pending['sync'] is None, 'Write after sync/overlapping write')
            pending['write'] = True
        elif kind == 'file_write':
            need(pending['write'], 'Write without transaction attempt')
            pending['write'] = False
            pending['bytes'] += integer(row['bytes'], 1, CAP, 'transaction written bytes')
            need(pending['bytes'] <= CAP, 'Transaction written byte cap')
        elif kind == 'sync_attempt':
            target = row['target']
            need(target in {'file', 'directory'} and pending['sync'] is None and not pending['write'] and
                 pending['bytes'] >= 16 and not pending[target] and
                 (target == 'file' or pending['rename']), 'Transaction sync order')
            pending['sync'] = target
        elif kind == 'sync_complete':
            target = row['target']
            need(target in {'file', 'directory'} and pending['sync'] == target, 'Sync lacks exact transaction attempt')
            pending['sync'] = None
            pending[target] = True
        elif kind == 'rename_complete':
            need(pending['file'] and not pending['rename'] and pending['sync'] is None, 'Rename before file sync/duplicate')
            pending['rename'] = True
        elif kind == 'disk_persisted':
            need(pending['file'] and pending['rename'] and pending['directory'] and
                 pending['sync'] is None and not pending['write'] and
                 row['file_sync'] is True and row['directory_sync'] is True and
                 type(row['bytes']) is int and row['bytes'] == pending['bytes'], 'Incomplete transaction completion')
            raw = content_delta(disk['raw'], extent(row, self.files))
            need(len(raw) == row['bytes'], 'Completed contents differ from written length')
            state = framed_state(raw)
            disk.update(raw=raw, completed=identity, token=dict(token), pending=None)
            return state
        return None

    def current(self, row, name):
        disk = self.disks.get(name)
        token = row.get('transaction')
        need(type(token) is dict and set(token)=={'disk','incarnation','id','predecessor'} and
             token['disk']==name, 'Declaration transaction shape/disk')
        integer(token['incarnation'],1,4096,'declaration incarnation')
        integer(token['id'],1,65536,'declaration transaction')
        integer(token['predecessor'],0,65536,'declaration predecessor')
        need(disk is not None and not disk['poisoned'] and disk['pending'] is None and
             token == disk['token'] and disk['token'] is not None,
             'Declaration lacks current completed transaction')
        return framed_state(disk['raw'])

    def finish(self):
        need(self.disks and all(d['pending'] is None for d in self.disks.values()), 'Incomplete transaction history')


def varint(raw, offset):
    number, start = 0, offset
    for shift in range(0, 70, 7):
        need(offset < len(raw), "Truncated protobuf varint")
        byte = raw[offset]
        offset += 1
        need(shift < 63 or byte <= 1, "Protobuf uint64 overflow")
        number |= (byte & 127) << shift
        if byte < 128:
            need(offset - start == 1 or byte != 0, "Noncanonical protobuf varint")
            return number, offset
    raise ValueError("Protobuf uint64 overflow")


def protobuf(raw, schema, repeated=()):
    need(type(raw) is bytes and len(raw) <= CAP, "Protobuf message byte bound")
    result, offset, count = {}, 0, 0
    while offset < len(raw):
        tag, offset = varint(raw, offset)
        field, wire = tag >> 3, tag & 7
        need(field in schema and wire in (schema[field] if type(schema[field]) is tuple else (schema[field],)),
             "Unsupported pinned protobuf field/wire type")
        need(field not in result or field in repeated, "Duplicate protobuf scalar field")
        if wire == 0:
            value, offset = varint(raw, offset)
        else:
            length, offset = varint(raw, offset)
            need(length <= CAP and offset + length <= len(raw), "Truncated protobuf bytes")
            value, offset = raw[offset:offset + length], offset + length
        result.setdefault(field, []).append(value)
        count += 1
        need(count <= 4096, "Protobuf field count bound")
    return result


def scalar(fields, field, default=0):
    return fields[field][0] if field in fields else default


def operation(row):
    need(type(row) is dict and set(row) == {"id", "tenant", "key", "stream", "payload"}, "Unknown native operation fields")
    identity = row["id"]
    need(type(identity) is str and 1 <= len(identity) <= 128 and identity.isascii(), "Native operation identity bound")
    tenant = integer(row["tenant"], 0, 3, "tenant")
    key = integer(row["key"], 0, 1, "key")
    stream = integer(row["stream"], 0, 15, "stream")
    prefix = f"id={identity};tenant={tenant};key={key};stream={stream};"
    need(row["payload"] == prefix + "#" * (1024 - len(prefix)), "Native payload identity/content mismatch")
    return identity, hashlib.sha256(row["payload"].encode("ascii")).hexdigest()


def batch(value):
    need(type(value) is dict and set(value) == {"operations"}, "Native batch shape")
    rows = value["operations"]
    need(type(rows) is list and 1 <= len(rows) <= 16, "Native logical batch bound")
    identities = [operation(row) for row in rows]
    need(len({(r["tenant"], r["key"]) for r in rows}) == 1, "Native batch mixed policy classes")
    need(len(set(identity for identity, _ in identities)) == len(identities), "Repeated native batch identity")
    return dict(identities)


def raft_message(raw, row):
    fields = protobuf(raw, {1:0, 2:0, 3:0, 4:0, 5:0, 6:0, 7:2, 8:0, 9:2, 10:0, 11:0, 12:2, 13:0, 14:0, 15:0, 16:0}, (7,))
    kind = integer(scalar(fields, 1), 0, 18, "message enum")
    need(MESSAGE_NAMES[kind] == row["message"] and scalar(fields, 2) == row["target"] and
         scalar(fields, 3) == row["source"], "Serialized protobuf edge identity differs from ledger")
    need(kind in {3, 4, 5, 6, 7, 8, 9, 13, 14, 15, 16, 17, 18}, "Local-only Raft event mislabeled network traffic")
    operations = {}
    need(len(fields.get(7, [])) <= 128, "Native entry count bound")
    for encoded in fields.get(7, []):
        entry = protobuf(encoded, {1:0, 2:0, 3:0, 4:2, 5:0, 6:2})
        entry_type = integer(scalar(entry, 1), 0, 2, "entry enum")
        data = scalar(entry, 4, b"")
        if entry_type == 0 and data:
            operations.update(batch(decode(data)))
        elif data:
            protobuf(data, {1:0, 2:0, 3:0, 4:2} if entry_type == 1 else {1:0, 2:2, 3:2}, (2,) if entry_type == 2 else ())
    if 9 in fields:
        snapshot = protobuf(scalar(fields, 9), {1:2, 2:2})
        metadata = protobuf(scalar(snapshot, 2, b""), {1:2, 2:0, 3:0})
        need(scalar(metadata, 2) > 0, "Snapshot has no applied prefix")
        application = decode(scalar(snapshot, 1, b""))
        need(application["applied"] == scalar(metadata, 2), "Snapshot applied prefix mismatch")
        operations.update(dict(operation(value) for value in application["operations"].values()))
    return operations


def openraft_message(raw, row):
    value = decode(raw)
    need(row["message"] in {"append_request", "append_response", "vote_request", "vote_response", "snapshot_request", "snapshot_response"},
         "Unknown OpenRaft RPC")
    operations = {}
    if row["message"] == "append_request":
        need(type(value) is dict and set(value) == {"vote", "prev_log_id", "entries", "leader_commit"}, "OpenRaft append shape")
        need(type(value["entries"]) is list and len(value["entries"]) <= 128, "OpenRaft entry count bound")
        for entry in value["entries"]:
            need(set(entry) == {"log_id", "payload"}, "OpenRaft entry shape")
            payload = entry["payload"]
            if type(payload) is dict and "Normal" in payload:
                need(set(payload) == {"Normal"}, "OpenRaft entry payload shape")
                operations.update(batch(payload["Normal"]))
    return operations


def load(root, manifest):
    root = Path(root)
    need(root.is_dir() and not root.is_symlink(), "Native artifact root missing/redirected")
    need(type(manifest) is list and 1 <= len(manifest) <= 256, "Native manifest file count")
    files, total = {}, 0
    for row in manifest:
        need(type(row) is dict and set(row) == {"file", "bytes", "sha256"}, "Native manifest fields")
        name = row["file"]
        need(type(name) is str and re.fullmatch(r"(artifacts/(events-[0-9]{3}\.ndjson|wire-[0-9]{3}\.bin)|node-[1-4](-(log|machine))?/state\.bin)", name),
             "Native artifact path shape")
        need(name not in files, "Duplicate native artifact path")
        size = integer(row["bytes"], 1, CAP, "artifact")
        need(re.fullmatch(r"[a-f0-9]{64}", row["sha256"]) is not None, "Native artifact hash shape")
        path = root / name
        need(path.resolve().is_relative_to(root.resolve()) and path.is_file() and not path.is_symlink() and
             path.stat().st_nlink == 1 and path.stat().st_size == size, "Native artifact size/ownership mismatch")
        with path.open("rb") as stream:
            raw = stream.read(CAP + 1)
        need(len(raw) == size and hashlib.sha256(raw).hexdigest() == row["sha256"], "Native artifact hash mismatch")
        files[name] = raw
        total += len(raw)
        need(total <= 32 * CAP, "Native artifact aggregate bound")
    actual = {p.relative_to(root).as_posix() for p in root.rglob("*") if not p.is_dir()}
    need(actual == set(files), "Native artifact inventory incomplete")
    return files


def verify(root, probe, candidate, *, history_kind="probe"):
    """Strict partial probe accounting only; no whole-task acceptance return value."""
    need(candidate in {"raft-rs", "openraft"}, "Unknown native candidate")
    need(history_kind in {"probe", "workload", "failure", "scale"}, "Unknown native history kind")
    files = load(root, probe["files"])
    event_names = sorted(name for name in files if "/events-" in name)
    wire_names = sorted(name for name in files if "/wire-" in name)
    need(event_names == [f"artifacts/events-{i:03}.ndjson" for i in range(len(event_names))] and
         wire_names == [f"artifacts/wire-{i:03}.bin" for i in range(len(wire_names))] and event_names and wire_names,
         "Native artifact segment gap")
    rows, actual_artifact_bytes = [], 0
    for name in event_names:
        raw = files[name]
        need(raw.endswith(b"\n") and b"\r" not in raw, "Native event framing")
        for line in raw.splitlines():
            need(0 < len(line) <= 4096, "Native event byte bound")
            rows.append(decode(line))
            need(len(rows) <= 131072, "Native event count bound")
    need([r.get("sequence") for r in rows] == list(range(len(rows))), "Native event sequence gap/duplicate")
    need(rows[0]["kind"] == "begin" and rows[0]["candidate"] == candidate, "Native begin binding")
    ends = [r for r in rows if r["kind"] == "end"]
    need(len(ends) == 1 and rows[-1] is ends[0], "Native events arrived after end or end missing/duplicate")
    report = probe["report"]
    actual_artifact_bytes = sum(len(files[name]) for name in (*event_names, *wire_names))
    need(report["artifact_bytes"] == actual_artifact_bytes, "Native final artifact total differs from actual bytes")
    need({k:v for k,v in report.items() if k != "artifact_bytes"} == ends[0]["result"], "Native end/report binding differs")
    integer(report["elapsed_us"], 1, 15000000, "history elapsed")
    cursor, edges, message_counts, ids = {name:0 for name in wire_names}, {}, Counter(), {}
    writes = write_calls = syncs = errors = 0
    pending_writes = set()
    pending_sync = set()
    applied = {}
    acknowledgements = []
    witnesses = []
    serialized_bytes = 0
    transactions = Transactions(files)
    for row in rows:
        kind = row["kind"]
        transactions.observe(row)
        if kind in {"wire", "state_witness", "disk_persisted", "disk_reopened"}:
            location = row["wire"]
            need(set(location) == {"file", "offset", "bytes"}, "Native wire location fields")
            name = "artifacts/" + location["file"]
            need(name in cursor and location["offset"] == cursor[name], "Native wire extent gap/overlap")
            size = integer(location["bytes"], 1, CAP, "wire")
            offset = integer(location["offset"], 0, CAP, "wire offset")
            need(offset + size <= len(files[name]), "Truncated native wire segment")
            raw = files[name][offset:offset + size]
            cursor[name] += size
            if kind in {"disk_persisted", "disk_reopened"}:
                continue
            if kind == "state_witness":
                need(re.fullmatch(r"node-[1-4](-(log|machine))?",row["directory"]) is not None and
                     type(row["label"]) is str and 1<=len(row["label"])<=64 and row["label"].isascii(),"Native state witness identity")
                witnesses.append({"sequence":row["sequence"],"directory":row["directory"],"label":row["label"],"state":framed_state(raw)})
                continue
            edge = integer(row["edge"], 1, 4096, "wire edge")
            need(edge not in edges, "Duplicate native wire edge")
            integer(row["source"], 1, 4, "source")
            integer(row["target"], 1, 4, "target")
            need(type(row["dropped"]) is bool, "Native drop classification")
            serialized_bytes += len(raw)
            observations = (raft_message if candidate == "raft-rs" else openraft_message)(raw, row)
            for identity, digest in observations.items():
                need(identity not in ids or ids[identity] == digest, "Native operation identity changed payload")
                ids[identity] = digest
            edges[edge] = row
            message_counts[row["message"]] += 1
        elif kind == "file_write":
            need(row["directory"] in pending_writes, "Native file write completed without attempt")
            pending_writes.remove(row["directory"])
            writes += integer(row["bytes"], 1, CAP, "actual file write")
        elif kind == "file_write_attempt":
            need(row["directory"] not in pending_writes, "Overlapping native file write")
            pending_writes.add(row["directory"])
            write_calls += 1
        elif kind == "sync_attempt":
            key = (row["directory"], row["target"])
            need(row["target"] in {"file", "directory"} and key not in pending_sync, "Overlapping native file sync")
            pending_sync.add(key)
            syncs += 1
        elif kind == "sync_complete":
            key = (row["directory"], row["target"])
            need(key in pending_sync, "Native sync completed without attempt")
            pending_sync.remove(key)
        elif kind == "disk_error":
            need(row["poisoned_until_reload"] is True, "Native IO error did not poison node")
            errors += 1
            pending_sync = {key for key in pending_sync if key[0] != row["directory"]}
            pending_writes.discard(row["directory"])
        elif kind == "applied":
            node = integer(row["node"], 1, 4, "applied node")
            integer(row["index"], 0, 4096, "applied index")
            need(row["application_synced"] is True and row["candidate_committed"] is True, "Native application omitted committed/synced coordinate")
            applied.setdefault(node, set()).update(row["ids"])
        elif kind == "client_ack":
            leader = row["leader"]
            copies = row["persisted_nodes"]
            need(type(copies) is list and len(set(copies)) == len(copies) and leader in copies and
                 all(type(node)is int and 1<=node<=4 for node in copies), "Native acknowledged without declared majority")
            voter_sets=row.get("voter_sets",[[1,2,3]])
            need(type(voter_sets)is list and 1<=len(voter_sets)<=2,"Native voter set count")
            for voters in voter_sets:
                need(type(voters)is list and 1<=len(voters)<=4 and len(set(voters))==len(voters) and
                     all(type(node)is int and 1<=node<=4 for node in voters) and
                     len(set(voters)&set(copies))>len(voters)//2,"Native acknowledged without declared majority")
            if history_kind!="failure":
                need(voter_sets==[[1,2,3]] and set(copies).issubset({1,2,3}),"Healthy native voter configuration changed")
            need(row["id"] in applied.get(leader,set()) and row["id"] in ids, "Native acknowledgement has no applied/serialized operation")
            acknowledgements.append(row["id"])
    need(not pending_sync and not pending_writes, "Native write/sync attempt unresolved at end")
    transactions.finish()
    for name, disk in transactions.disks.items():
        need(files.get(name + '/state.bin') == disk['raw'], 'Final disk differs from completed content history')
    need(sorted(edges) == list(range(1,len(edges)+1)), "Native missing wire attempt")
    need(all(cursor[name] == len(files[name]) for name in wire_names), "Native unaccounted serialized bytes")
    counts = report["counts"]
    expected = {"attempts":len(edges), "serialized_bytes":serialized_bytes, "dropped":sum(r["dropped"] for r in edges.values()),
                "file_write_bytes":writes, "file_write_calls":write_calls, "sync_calls":syncs, "io_errors":errors}
    need(all(type(counts.get(k)) is int and counts[k] == v for k,v in expected.items()), "Native independent wire/write/sync/error accounting differs")
    integer(counts["write_reserved_bytes"], 0, 512 * CAP, "write reservation")
    integer(counts["sync_reserved_calls"], 0, 65536, "sync reservation")
    need(writes <= counts["write_reserved_bytes"] <= 512 * CAP and syncs <= counts["sync_reserved_calls"] <= 65536 and write_calls<=65536,
         "Native atomic write/sync reservation bound")
    need(counts["serialized_bytes"] <= 16 * CAP and counts["attempts"] <= 4096, "Native history transport budget")
    need(acknowledgements, "Native history has no acknowledged operation")
    if history_kind == "probe":
        need(any(r["kind"] == "persistence_barrier_reached" for r in rows) and
             any(r["kind"] == "graceful_reopen_verified" for r in rows), "Native partial probe history missing")
    elif history_kind == "workload":
        need(sum(r["kind"] == "offered_begin" for r in rows) == 1 and
             sum(r["kind"] == "offered_complete" for r in rows) == 1,
             "Native matched offered history missing")
    elif history_kind == "failure":
        need(sum(r["kind"]=="failure_begin" for r in rows)==1 and sum(r["kind"]=="failure_complete" for r in rows)==1 and witnesses,
             "Native complete failure history missing")
    else:
        need(sum(r["kind"]=="scale_group_begin" for r in rows)==1 and sum(r["kind"]=="scale_group_stopped" for r in rows)==1,
             "Native active-group history missing")
    if candidate == "openraft":
        need(any(r["kind"] == "application_drain_barrier" for r in rows), "OpenRaft lacks applied drain")
        if history_kind in {"probe","failure"}:
            need(rows[-2]["kind"] == "test_runtime_stopped", "OpenRaft summary lacks applied/runtime drain")
        else:
            stopped = [i for i,r in enumerate(rows) if r["kind"] == "test_runtime_stopped"]
            trailing={"scale_group_stopped","end"} if history_kind=="scale" else {"driver_stopped","offered_complete","end"}
            need(len(stopped) == 1 and all(r["kind"] in trailing for r in rows[stopped[0]+1:]),
                 "OpenRaft workload continued after runtime stop")
    return {"native_probe_accounting":"pass", "candidate":candidate, "counts":expected,
            "messages":dict(message_counts), "operation_payloads":ids, "acknowledged":acknowledgements,
            "actual_artifact_bytes":actual_artifact_bytes, "files":len(files),"witnesses":witnesses,
            "scope":"Strict retained-byte/local probe accounting; full M02.10 matrix/source/coverage/reviews not accepted"}
