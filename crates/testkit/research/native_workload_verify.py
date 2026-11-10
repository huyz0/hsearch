"""Recompute matched offered schedules, outcomes and final voter bytes independently."""
from collections import Counter
from pathlib import Path
import runpy

ROOT = Path(__file__).resolve().parents[3]
V = runpy.run_path(str(Path(__file__).with_name("native_verify.py")))
MODEL = runpy.run_path(str(ROOT / "tools/durability_research.py"))
need, integer, decode, protobuf, scalar = (V[name] for name in ("need", "integer", "decode", "protobuf", "scalar"))


def state(raw):
    need(len(raw) >= 16 and raw[:8] == b"HSRCHD01" and int.from_bytes(raw[8:16], "little") == len(raw)-16,
         "Native final voter state framing")
    return decode(raw[16:])


def bytes_value(value):
    need(type(value) is dict and len(value) == 1, "Native stored data shape")
    if "Utf8" in value:
        need(type(value["Utf8"]) is str, "Native stored UTF8 type")
        return value["Utf8"].encode("utf-8")
    need(set(value) == {"Binary"} and type(value["Binary"]) is list and len(value["Binary"]) <= 1048576 and
         all(type(byte) is int and 0 <= byte <= 255 for byte in value["Binary"]), "Native stored binary type/bound")
    return bytes(value["Binary"])


def packed(raw):
    values, offset = [], 0
    while offset < len(raw):
        number, offset = V["varint"](raw, offset)
        values.append(integer(number, 1, 4, "native member"))
        need(len(values) <= 4, "Native membership bound")
    need(len(set(values)) == len(values), "Duplicate native member")
    return values


def members(fields,field):
    # Both expanded and packed repeated integer forms are valid:
    # https://protobuf.dev/programming-guides/encoding/#packed
    values=[]
    for value in fields.get(field,[]):
        values.extend(packed(value) if type(value) is bytes else [integer(value,1,4,"native member")])
    need(len(values)<=4 and len(values)==len(set(values)),"Native membership duplicate/count bound")
    return values


def healthy_log_id(value):
    need(type(value)is dict and set(value)=={'leader_id','index'} and type(value['leader_id'])is dict and
         set(value['leader_id'])=={'term','node_id'},'OpenRaft healthy log identity shape')
    integer(value['leader_id']['term'],0,2**64-1,'healthy log term');integer(value['leader_id']['node_id'],0,3,'healthy log node')
    return integer(value['index'],0,255,'healthy log index')


def healthy_openraft(log,machine):
    """The healthy workload has one initial membership and no snapshots or elections."""
    membership={'configs':[[1,2,3]],'nodes':{'1':None,'2':None,'3':None}}
    initial={'leader_id':{'term':0,'node_id':0},'index':0}
    need(set(machine)=={'applied','snapshot'} and machine['snapshot']is None,'Healthy OpenRaft snapshot/machine changed')
    app=machine['applied'];need(type(app)is dict and set(app)=={'last','membership','operations'},'OpenRaft healthy application shape')
    need(app['membership']=={'log_id':initial,'membership':membership},'Healthy OpenRaft voter membership changed')
    healthy_log_id(app['membership']['log_id'])
    need(all(type(n)is int for n in app['membership']['membership']['configs'][0]),'Healthy OpenRaft member type')
    need(set(log)=={'vote','purged','committed','entries'} and log['purged']is None,'OpenRaft healthy final log shape')
    vote=log['vote'];need(type(vote)is dict and set(vote)=={'leader_id','committed'} and vote['committed']is True,'OpenRaft healthy persisted vote')
    leader=vote['leader_id'];need(type(leader)is dict and set(leader)=={'term','node_id'},'Healthy OpenRaft leader identity')
    integer(leader['term'],1,2**64-1,'healthy leader term');integer(leader['node_id'],1,3,'healthy leader node')
    entries=log['entries'];need(type(entries)is dict and 2<=len(entries)<=256 and
        all(type(key)is str and key.isascii() and key.isdecimal() and str(int(key))==key for key in entries),'OpenRaft healthy log map keys')
    indices=sorted(map(int,entries));need(indices==list(range(len(indices))),'OpenRaft healthy log prefix')
    need(entries['0']=={'log_id':initial,'payload':{'Membership':membership}},'OpenRaft healthy initial configuration Entry')
    healthy_log_id(entries['0']['log_id'])
    need(all(type(n)is int for n in entries['0']['payload']['Membership']['configs'][0]),'Healthy initial member type')
    for index in indices[1:]:
        entry=entries[str(index)];need(type(entry)is dict and set(entry)=={'log_id','payload'} and
            entry['log_id']=={'leader_id':leader,'index':index},'OpenRaft healthy Entry identity')
        need(healthy_log_id(entry['log_id'])==index,'OpenRaft healthy Entry index')
        payload=entry['payload'];need(payload=='Blank' or type(payload)is dict and set(payload)=={'Normal'},'OpenRaft healthy Entry kind')
    last=entries[str(indices[-1])]['log_id']
    healthy_log_id(app['last']);healthy_log_id(log['committed'])
    need(app['last']==log['committed']==last,'OpenRaft healthy committed/applied coordinate differs')
    return indices[-1]


def timing(beginning,outcome):
    origin=integer(beginning['origin_us'],0,15000000,'offered origin')
    need(integer(outcome['offered_origin_us'],0,15000000,'outcome origin')==origin,'Native outcome origin changed')
    duration=integer(outcome['observed_duration_us'],1,15000000,'observed offered duration')
    integer(outcome['peak_resident_records'],1,512,'peak resident records')
    integer(outcome['peak_encoded_pending_bytes'],1,1048576,'peak encoded bytes')
    return origin,duration


def final_states(files, candidate, expected, published, applied):
    for node in (1,2,3):
        if candidate == "raft-rs":
            saved = state(files[f"node-{node}/state.bin"])
            need(set(saved) == {"schema_version", "hard_state", "entries", "conf_state", "snapshot", "application"} and
                 saved["schema_version"] == 2 and saved["snapshot"] == [], "Native workload scratch schema/snapshot")
            app = saved["application"]
            need(set(app) == {"applied", "operations"}, "Native final application shape")
            hard = protobuf(bytes(saved["hard_state"]), {1:0,2:0,3:0})
            conf = protobuf(bytes(saved["conf_state"]), {1:(0,2),2:(0,2),3:(0,2),4:(0,2),5:0}, (1,2,3,4))
            need(members(conf,1) == [1,2,3] and not any(field in conf for field in (2,3,4,5)), "Healthy workload voter membership changed")
            log_ids, previous, log_payloads = [], 0, {}
            for entry in saved["entries"]:
                need(set(entry) == {"kind", "term", "index", "data", "context", "sync_log"}, "Native stored Entry fields")
                index = integer(entry["index"],1,256,"stored Entry index")
                need(index == previous+1 and entry["kind"] == 0 and type(entry["sync_log"]) is bool,
                     "Native stored Entry hole/kind")
                need(entry["context"] == [] and entry["sync_log"] is False, "Healthy workload Entry context changed")
                need(integer(entry["term"],1,2**64-1,"stored term") <= scalar(hard,1), "Native log term exceeds persisted HardState")
                raw = bytes_value(entry["data"])
                if raw:
                    payloads=V["batch"](decode(raw))
                    need(not set(payloads).intersection(log_payloads), "Healthy workload repeated an operation log")
                    log_payloads.update(payloads)
                log_ids.append(index)
                previous=index
            need(log_ids and app["applied"] == log_ids[-1] and scalar(hard,3) <= log_ids[-1], "Native final log/applied/HardState coordinate")
            need(log_payloads == {identity:expected[identity] for identity in published}, "Native final durable log differs from published payloads")
            applied_index = app["applied"]
        else:
            machine = state(files[f"node-{node}-machine/state.bin"])
            app=machine["applied"]
            log=state(files[f"node-{node}-log/state.bin"])
            applied_index=healthy_openraft(log,machine)
            entries=log["entries"]
            payloads={}
            for entry in entries.values():
                payload=entry["payload"]
                if type(payload) is dict and "Normal" in payload:
                    values=V["batch"](payload["Normal"])
                    need(not set(values).intersection(payloads), "OpenRaft healthy workload duplicated operation")
                    payloads.update(values)
            need(payloads == {identity:expected[identity] for identity in published}, "OpenRaft durable log differs from published payloads")
        need(type(app["operations"]) is dict and all(key == value["id"] for key,value in app["operations"].items()),
             "Native durable operation map key/identity differs")
        actual=dict(V["operation"](value) for value in app["operations"].values())
        need(actual == {identity:expected[identity] for identity in published} and
             set(app["operations"]) == published and applied_index == applied[node], "Native final durable application differs from applied/published ledger")


def verify_case(root, probe, candidate, trace, wait_us):
    base=V["verify"](root,probe,candidate,history_kind="workload")
    files=V["load"](root,probe["files"])
    rows=[decode(line) for name in sorted(files) if "/events-" in name for line in files[name].splitlines()]
    beginning=next(row for row in rows if row["kind"] == "offered_begin")
    outcome=next(row["result"] for row in rows if row["kind"] == "offered_complete")
    origin,duration=timing(beginning,outcome)
    need(probe["case"] == outcome and beginning["workload"] == trace["workload"] == outcome["workload"] and
         beginning["seed"] == trace["seed"] == outcome["seed"] and beginning["wait_us"] == wait_us == outcome["wait_us"] and
         beginning["grouping"] == outcome["grouping"] == "compatible", "Native matched case binding")
    schedule=MODEL["batches"](trace,"compatible",wait_us)
    need(beginning["planned_batches"] == outcome["planned_batches"] == len(schedule) and
         beginning["offered_records"] == outcome["offered"] == len(trace["events"]), "Native fixed offered cardinality")
    original={event["id"]:event for event in trace["events"]}
    expected={identity:event["payload_sha256"] for identity,event in original.items()}
    offers=[row for row in rows if row["kind"] == "offered_batch"]
    need([row["ticket"] for row in offers] == list(range(len(schedule))), "Native offered schedule gap/duplicate")
    completed={}
    resident={}
    peak=0
    per_lane_order={}
    last_applied={}
    for row in rows:
        if row["kind"] == "offered_batch":
            planned=schedule[integer(row['ticket'],0,len(schedule)-1,'offered ticket')]
            need(row["ids"] == planned["ids"] and row["planned_emit_us"] == planned["emit_us"] and
                 row["origin_us"] == beginning["origin_us"] and type(row["admitted"]) is bool,
                 "Native substituted offered schedule")
            integer(row["origin_us"],origin,origin,'actual offer origin')
            integer(row["planned_emit_us"],planned["emit_us"],planned["emit_us"],'planned offer')
            integer(row["observed_emit_us"],planned["emit_us"],duration,"actual offer")
            lane=tuple(planned["lane"])
            if row["admitted"]:
                resident[lane]=resident.get(lane,0)+len(row["ids"])
                need(resident[lane] <= 32 and sum(resident.values()) <= 512, "Native resident admission exceeded bound")
                peak=max(peak,sum(resident.values()))
        elif row["kind"] == "batch_completed":
            ticket=integer(row["ticket"],0,len(schedule)-1,"completed ticket")
            need(ticket not in completed and offers[ticket]["admitted"], "Native duplicate/unadmitted completion")
            planned=schedule[ticket]
            for key in ('enqueued_us','started_us','completed_us'):
                integer(row[key],0,duration,'completion '+key)
            integer(row['origin_us'],origin,origin,'completion origin')
            integer(row['planned_emit_us'],planned['emit_us'],planned['emit_us'],'planned completion')
            need(row["ids"] == planned["ids"] and row["lane"] == planned["lane"] and row["trigger"] == planned["trigger"] and
                 row["planned_emit_us"] == planned["emit_us"] and row["offered_us"] == [original[i]["at_us"] for i in planned["ids"]] and
                 row["enqueued_us"] == offers[ticket]["observed_emit_us"] and row["origin_us"] == beginning["origin_us"] and
                 row["completed_us"] >= row["started_us"] >= row["enqueued_us"], "Native completion timing/content differs from fixed offer")
            lane=tuple(planned["lane"])
            resident[lane]-=len(row["ids"])
            need(resident[lane] >= 0, "Native completion released nonexistent resident data")
            per_lane_order.setdefault(lane,[]).append(ticket)
            completed[ticket]=row
        elif row["kind"] == "applied":
            node=row["node"]
            need(node not in last_applied or row["index"] == last_applied[node]+1, "Native applied prefix skipped/reordered")
            last_applied[node]=row["index"]
    published={identity for row in completed.values() for identity in row["ids"]}
    rejected={identity for row in offers if not row["admitted"] for identity in row["ids"]}
    need(published and published.isdisjoint(rejected) and published|rejected == set(original) and
         sorted(outcome["published"]) == sorted(published) and sorted(outcome["rejected"]) == sorted(rejected) and
         outcome["unknown"] == [] and not any(resident.values()), "Native offered outcome conservation")
    for lane,tickets in per_lane_order.items():
        need(tickets == sorted(tickets), "Native per-lane FIFO completion changed")
    need(base["operation_payloads"] == {identity:expected[identity] for identity in published} and
         Counter(base["acknowledged"]) == Counter(published), "Native wire/ack payloads differ from completed ledger")
    # Producer-time peaks can precede delayed event reporting; source reports must be conservative.
    need(peak <= outcome["peak_resident_records"] <= 512 and outcome["peak_encoded_pending_bytes"] <= 1048576,
         "Native resident peak underreported or above budget")
    need(set(last_applied) == {1,2,3}, "Native final voter application missing")
    final_states(files,candidate,expected,published,last_applied)
    return {"native_workload_case":"pass","candidate":candidate,"workload":trace["workload"],"seed":trace["seed"],"wait_us":wait_us,
            "offered":len(original),"published":len(published),"rejected":len(rejected),"counts":base["counts"],
            "artifact_bytes":base["actual_artifact_bytes"],"scope":"Matched component/state/byte accounting only; full M02.10 qualification remains"}
