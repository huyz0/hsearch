"""Run bounded reference fixtures in one owned network-none OpenSearch container."""
from pathlib import Path
import argparse
import tempfile
import hashlib
import json
import math
import re
import runpy
import sys
import threading
import time
from urllib.parse import quote

ROOT = Path(__file__).resolve().parents[1]
API = runpy.run_path(str(ROOT / "tools/api_fixtures.py"))
SANDBOX = runpy.run_path(str(ROOT / "tools/sandbox.py"))
TASK = runpy.run_path(str(ROOT / "tools/task.py"))
LIMITS = runpy.run_path(str(ROOT / "dev/execute.py"))
need, Refused = SANDBOX["need"], SANDBOX["Refused"]


def owned_identity(info, owner, namespace, cid, image_id):
    need(info["Id"] == cid and info["Image"] == image_id and
         info["Config"]["Labels"].get(SANDBOX["LABEL"]) == owner and
         info["Config"]["Labels"].get("io.hsearch.namespace") == namespace,
         "Reference identity/ownership changed; preserve resources")


def check_caps(info):
    host = info["HostConfig"]
    need(host["NetworkMode"] == "none" and not host["PortBindings"] and not host["PublishAllPorts"] and
         host["NanoCpus"] == 2000000000 and host["Memory"] == 4 * SANDBOX["GIB"] and
         host["MemorySwap"] == host["Memory"] and host["PidsLimit"] == 512 and
         host.get("Privileged") is False and host.get("PidMode", "") == "" and
         "ALL" in host["CapDrop"] and "no-new-privileges" in host["SecurityOpt"] and
         info["Config"]["User"] == "1000:1000" and info["Config"]["Tty"] is True and
         all(item["Type"] == "tmpfs" for item in info["Mounts"]), "Reference caps/isolation changed")


def cleanup_failed(sandbox, namespace):
    need(SANDBOX["NAMESPACE"].fullmatch(namespace), "Invalid reference namespace")
    record = sandbox.registry.get(namespace)
    need(record.get("kind") == "api-reference" and record.get("source") == str(sandbox.source_root) and
         record["engine"] == sandbox.docker.engine, "Reference cleanup belongs to another source/engine")
    owner = SANDBOX["owner_identity"](record["pid"])
    need(owner is None or owner != "unknown" and owner != record["process_identity"], "Reference owner is live or uncertain")
    cid = record.get("container_id")
    need(type(cid) is str and re.fullmatch(r"[a-f0-9]{64}", cid) and not record.get("create_pending"), "Unknown create outcome; inspect retained operation")
    info = json.loads(sandbox.docker.command("container", "inspect", cid, timeout=10, output_bytes=32768))[0]
    owned_identity(info, record["owner"], namespace, cid, record["reference_image"])
    need(not info["State"]["Running"] and info["State"]["Status"] in {"created", "exited", "dead"}, "Known reference is still running; wait for its bounded lifetime")
    need(record.get("evidence_captured") is True, "Capture bounded failed-reference evidence before removal")
    sandbox.docker.command("container", "rm", cid, timeout=10, output_bytes=4096)
    need(not sandbox.docker.command("container", "ls", "--all", "--no-trunc", "--filter", "id=" + cid, "--format", "{{.ID}}", timeout=10, output_bytes=4096).strip(), "Reference removal uncertain; lease retained")
    sandbox.registry.release(namespace)
    return {"reference_cleanup": "pass", "namespace": namespace, "container_id": cid, "force": False}


def same_json(left, right):
    return json.dumps(left, sort_keys=True, separators=(",", ":"), allow_nan=False) == json.dumps(right, sort_keys=True, separators=(",", ":"), allow_nan=False)


def pointer(value, path):
    need(type(path) is str and re.fullmatch(r"(?:/[A-Za-z0-9_]+){1,16}", path), "Invalid bounded response pointer")
    for part in path.split("/")[1:]:
        if type(value) is dict:
            need(part in value, "Missing reference response field: " + path)
            value = value[part]
        else:
            need(type(value) is list and part.isdigit() and int(part) < len(value), "Invalid reference response array: " + path)
            value = value[int(part)]
    return value


def bind_value(value, bindings, depth=0):
    need(depth <= 16, "Reference template nesting exceeds bound")
    if type(value) is dict:
        return {key: bind_value(item, bindings, depth + 1) for key, item in value.items()}
    if type(value) is list:
        return [bind_value(item, bindings, depth + 1) for item in value]
    if type(value) is str:
        match = re.fullmatch(r"\{([a-z][a-z0-9_]*)\}", value)
        if match:
            need(match[1] in bindings, "Unknown reference response capture")
            return bindings[match[1]]
    return value


def bind_path(path, bindings):
    def replacement(match):
        need(match[1] in bindings and type(bindings[match[1]]) in {str, int}, "Invalid path response capture")
        return quote(str(bindings[match[1]]), safe="")
    result = re.sub(r"\{([a-z][a-z0-9_]*)\}", replacement, path)
    need(len(result.encode()) <= 4096 and "{" not in result and "}" not in result, "Unbound/oversized reference path")
    return result


def capture_value(value):
    values = value if type(value) is list else [value]
    need(1 <= len(values) <= 16 and all(type(item) in {str, int} for item in values) and
         len(json.dumps(value).encode()) <= 16384, "Unbounded or invalid response capture")
    return value


def verify_step(expected, status, body, bindings):
    need(status == expected["status"], f"Reference HTTP oracle mismatch: expected {expected['status']}, observed {status}")
    for path, value in expected.get("fields", {}).items():
        need(same_json(pointer(body, path), bind_value(value, bindings)), "Reference field oracle mismatch: " + path)
    if expected.get("error_required"):
        need(type(body.get("error")) is dict and body["error"], "Reference failure must carry a typed error")
    for path in expected.get("nonempty_strings", []):
        value = pointer(body, path)
        need(type(value) is str and 0 < len(value.encode()) <= 8192, "Reference receipt/token is absent or oversized")
    for path in expected.get("nonnegative_integers", []):
        value = pointer(body, path)
        need(type(value) is int and 0 <= value < 2**63, "Reference sequence/term must be a bounded integer")
    if "hit_ids" in expected:
        need([item["_id"] for item in body["hits"]["hits"]] == expected["hit_ids"], "Reference history hit order/identity mismatch")
    if "exact_total" in expected:
        need(same_json(body["hits"]["total"], {"value": expected["exact_total"], "relation": "eq"}), "Reference history exact total is not complete")
    if "truthful_total" in expected:
        total, oracle = body["hits"]["total"], expected["truthful_total"]
        need(type(total["value"]) is int and
             (total["relation"] == "eq" and total["value"] == oracle["exact"] or
              total["relation"] == "gte" and oracle["threshold"] <= total["value"] <= oracle["exact"]),
             "Reference count relation/value is not truthful")


def verify_search(case, status, body):
    need(status == 200, "Reference query failed: " + case["id"])
    oracle = case["reference_oracle"]
    if oracle["kind"] == "hits":
        hits = body["hits"]
        need(same_json([item["_id"] for item in hits["hits"]], oracle["ids"]) and
             same_json(hits["total"], {"value": oracle["exact_total"], "relation": "eq"}),
             "Reference ID/count/order oracle mismatch: " + case["id"])
        need(all(same_json(item["_source"], {"uid": item["_id"]}) for item in hits["hits"]),
             "Reference source projection mismatch")
    else:
        for name, expected in oracle["values"].items():
            observed = body["aggregations"][name]
            if type(expected) is list:
                need(same_json([{key: item[key] for key in ("key", "doc_count")} for item in observed["buckets"]], expected),
                     "Reference bucket oracle mismatch")
            else:
                value = observed["value"]
                # OpenSearch metrics may serialize an integral value as a double.
                # Booleans and nonfinite values never represent numeric metrics.
                need(type(value) in {int, float} and type(expected) in {int, float} and value == expected and
                     math.isfinite(value) and math.isfinite(expected), "Reference metric oracle mismatch")


def verify_analyzer(case, status, body):
    need(status == 200 and same_json([{key: item[key] for key in ("token", "position", "start_offset", "end_offset")}
                                     for item in body["tokens"]], case["tokens"]),
         "Reference analyzer oracle mismatch: " + case["id"])


def source_identity():
    head = TASK["git"]("rev-parse", "HEAD", root=ROOT).strip()
    state = TASK["source_fingerprint"](ROOT, head, [])
    need(state["clean"], "Reference execution requires actual clean committed source")
    return {"head": head, "tree": TASK["git"]("rev-parse", "HEAD^{tree}", root=ROOT).strip(), "source_digest": state["digest"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-root", type=Path, default=Path(tempfile.gettempdir()) / "hsearch-dev-leases")
    parser.add_argument("--evidence-parent", type=Path, default=Path(tempfile.gettempdir()) / "hsearch-api-reference")
    parser.add_argument("action", choices=("run", "cleanup"), nargs="?", default="run")
    parser.add_argument("namespace", nargs="?")
    args = parser.parse_args()
    area = args.evidence_parent.resolve()
    need(not area.is_relative_to(ROOT) and not args.state_root.resolve().is_relative_to(ROOT), "Reference state/evidence must stay outside source")
    if args.action == "cleanup":
        need(args.namespace, "Reference cleanup requires its exact namespace")
        print(json.dumps(cleanup_failed(SANDBOX["Sandbox"](args.state_root, source_root=ROOT), args.namespace)), flush=True)
        return
    need(args.namespace is None, "Reference run does not accept an external target")
    controller_bytes = Path(__file__).read_bytes()
    identity = source_identity()
    integrity, inputs = API["checked_inputs"]()
    need(source_identity() == identity, "Source changed while freezing reference inputs")
    sb = SANDBOX["Sandbox"](args.state_root, source_root=ROOT)
    dev_image = sb.image()
    namespace = sb.namespace("api-reference")
    owner = "M02.01-reference-" + namespace.rsplit("-", 1)[1]
    folder = area / namespace
    folder.mkdir(parents=True, exist_ok=False)
    manifest = inputs["manifest"]
    architecture = sb.docker.info["Architecture"]
    key = {"x86_64": "linux_amd64", "aarch64": "linux_arm64"}.get(architecture)
    need(key is not None, "Unqualified native reference architecture")
    reference = "opensearchproject/opensearch@" + manifest["reference"][key]
    record = {"namespace": namespace, "owner": owner, "source": str(ROOT), "image": dev_image,
              "kind": "api-reference", "phase": "preflight", "container_id": None,
              "operation_pending": True, "create_pending": False, "stop_pending": False, "owner_active": True,
              "evidence_captured": False, "evidence_directory": str(folder)}
    sb.registry.acquire(namespace, SANDBOX["BUILD"], sb.docker.capacity, sb.docker.engine, record)
    cid, monitor = None, None
    done, violations = threading.Event(), []
    results, peak, pull_growth = [], 0, 0

    def command(*args, timeout=10, limit=131072):
        return sb.docker.command(*args, timeout=timeout, output_bytes=limit)

    def owned():
        info = json.loads(command("container", "inspect", "--size", cid, limit=32768))[0]
        owned_identity(info, owner, namespace, cid, image_id)
        check_caps(info)
        return info

    def monitor_workload():
        nonlocal peak
        started = time.monotonic()
        try:
            while not done.is_set():
                info = owned()
                peak = max(peak, info["SizeRw"] + pull_growth + 1024**2)
                need(peak <= SANDBOX["BUILD"]["disk_bytes"], "Sampled reference disk budget exceeded")
                need(time.monotonic() - started < 300, "Reference execution deadline exceeded")
                need(info["State"]["Running"], "Reference process exited before qualification")
                free = command("exec", cid, "df", "-Pk", "/", limit=4096)
                free_bytes = int(free.splitlines()[-1].split()[3]) * 1024
                sb.require_backend_reserve(free_bytes)
                done.wait(1)
        except Exception as error:
            violations.append(str(error)[:256])
            try:
                if owned()["State"]["Running"]:
                    sb.registry.update(namespace, stop_pending=True)
                    command("kill", cid, limit=4096)
                    sb.registry.update(namespace, stop_pending=False)
            except Exception as stop_error:
                violations.append("Stop outcome requires inspection: " + str(stop_error)[:200])
            done.set()

    def request(method, path, body=None, content_type="application/json", *, health=False):
        need(not violations, "Reference monitor refused execution: " + repr(violations))
        need(path.startswith("/") and not any(item in path for item in ("://", "..", "\n", "\r")), "Unsafe reference path")
        # A positive shell acknowledgement carries curl's own exit code. A
        # missing Docker acknowledgement is never treated as readiness failure.
        args = ["exec", cid, "/bin/bash", "-c", 'curl "$@"; code=$?; printf "\\nHSEARCH_CURL_EXIT=%s\\n" "$code"',
                "hsearch-curl", "--silent", "--show-error", "--max-time", "2" if health else "10",
                "--request", method, "--write-out", "\n%{http_code}", "--header", "Content-Type: " + content_type]
        if body is not None:
            wire = body if isinstance(body, str) else json.dumps(body, ensure_ascii=False, separators=(",", ":"))
            need(len(wire.encode()) <= 16384, "Reference request exceeds toy-corpus admission")
            args += ["--data-binary", wire]
        args += ["http://127.0.0.1:9200" + path]
        sb.registry.update(namespace, request_pending={"method": method, "path": path})
        raw = command(*args, timeout=5 if health else 15)
        raw, exit_text = raw.rsplit("\nHSEARCH_CURL_EXIT=", 1)
        exit_text = exit_text.strip()
        need(re.fullmatch(r"[0-9]{1,3}", exit_text) is not None, "Curl completion is unacknowledged")
        event = {"method": method, "path": path, "curl_exit": int(exit_text), "wire": raw}
        wire = json.dumps(event, ensure_ascii=False) + "\n"
        observations = folder / "acknowledged-http.jsonl"
        need((observations.stat().st_size if observations.exists() else 0) + len(wire.encode()) <= 1024**2,
             "Reference acknowledgement evidence budget exceeded")
        with observations.open("a", encoding="utf-8", newline="\n") as log:
            log.write(wire)
            log.flush()
        if int(exit_text):
            if health:
                sb.registry.update(namespace, request_pending=None)
                return None
            raise Refused("Reference HTTP outcome uncertain; acknowledged curl exit=" + exit_text)
        sb.registry.update(namespace, request_pending=None)
        body_text, status = raw.rsplit("\n", 1)
        need(re.fullmatch(r"[0-9]{3}", status) is not None, "Unacknowledged reference HTTP status")
        value = json.loads(body_text)
        results.append({"method": method, "path": path, "status": int(status), "body": value})
        need(len(json.dumps(results).encode()) <= 1024**2, "Reference observation budget exceeded")
        return int(status), value

    try:
        free_before = sb.backend_free(record, namespace + "-admission")
        sb.registry.update(namespace, operation_pending=False)
        sb.require_backend_reserve(free_before)
        sb.registry.update(namespace, operation_pending=True, phase="pulling")
        command("pull", reference, timeout=600, limit=1024**2)
        sb.registry.update(namespace, operation_pending=False)
        sb.registry.update(namespace, operation_pending=True)
        free_after = sb.backend_free(record, namespace + "-after-pull")
        sb.registry.update(namespace, operation_pending=False)
        sb.require_backend_reserve(free_after)
        pull_growth = max(0, free_before - free_after)
        image = json.loads(command("image", "inspect", reference, limit=32768))[0]
        image_id = image["Id"]
        need(image["Os"] == "linux" and image["Architecture"] == ("amd64" if key == "linux_amd64" else "arm64"), "Reference image platform mismatch")
        need(set(image["Config"].get("Volumes") or {}).issubset({"/usr/share/opensearch/data"}), "Unexpected image-declared volume")
        args = ["create", "--tty", "--name", namespace, "--hostname", "hsearch-reference",
                "--add-host", "hsearch-reference:127.0.0.1", "--label", SANDBOX["LABEL"] + "=" + owner,
                "--label", "io.hsearch.namespace=" + namespace, "--network", "none", "--user", "1000:1000",
                "--cpus", "2", "--memory", str(4 * SANDBOX["GIB"]), "--memory-swap", str(4 * SANDBOX["GIB"]),
                "--pids-limit", "512", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                "--ulimit", "nofile=65536:65536", "--log-driver", "local", "--log-opt", "max-size=1m", "--log-opt", "max-file=1", "--log-opt", "compress=false",
                "--tmpfs", "/usr/share/opensearch/data:rw,noexec,nosuid,nodev,size=134217728,uid=1000,gid=1000",
                "--tmpfs", "/tmp:rw,noexec,nosuid,nodev,size=67108864,uid=1000,gid=1000",
                "--tmpfs", "/usr/share/opensearch/ref-tmp:rw,exec,nosuid,nodev,size=67108864,uid=1000,gid=1000"]
        for setting in ("DISABLE_INSTALL_DEMO_CONFIG=true", "DISABLE_SECURITY_PLUGIN=true", "discovery.type=single-node",
                        "network.host=127.0.0.1", "OPENSEARCH_TMPDIR=/usr/share/opensearch/ref-tmp",
                        "JAVA_TOOL_OPTIONS=-Djava.io.tmpdir=/usr/share/opensearch/ref-tmp",
                        "OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m"):
            args += ["--env", setting]
        args += ["--entrypoint", "/usr/bin/timeout", image_id, "--signal=TERM", "--kill-after=5s", "300s",
                 "/usr/share/opensearch/opensearch-docker-entrypoint.sh", "opensearch"]
        sb.registry.update(namespace, create_pending=True, phase="creating", reference_image=image_id)
        candidate = command(*args, timeout=30, limit=4096).strip()
        need(re.fullmatch(r"[0-9a-f]{64}", candidate) is not None, "Unknown create outcome; lease retained")
        cid = candidate
        sb.registry.update(namespace, container_id=cid, create_pending=False)
        owned()
        command("start", cid, timeout=15, limit=4096)
        sb.registry.update(namespace, phase="running")
        mounts = command("exec", cid, "/bin/bash", "-c",
                         'while IFS=" " read -r source target kind options rest; do case "$target" in /tmp|/usr/share/opensearch/ref-tmp|/usr/share/opensearch/data) printf "%s %s\\n" "$target" "$options";; esac; done < /proc/mounts', limit=4096)
        (folder / "reference-mount-options.txt").write_text(mounts, encoding="utf-8", newline="\n")
        limit_text = command("exec", cid, "/bin/bash", "-c",
                             'for name in cpu.max memory.max memory.swap.max pids.max; do printf "%s " "$name"; cat "/sys/fs/cgroup/$name"; done; printf "uid "; id -u', limit=4096)
        actual_limits = dict(line.split(" ", 1) for line in limit_text.splitlines())
        LIMITS["verify_limits"](2000, 4 * SANDBOX["GIB"], 512, limits=actual_limits, uid=int(actual_limits["uid"]))
        (folder / "actual-cgroup-limits.json").write_text(json.dumps(actual_limits, indent=2) + "\n", encoding="utf-8", newline="\n")
        monitor = threading.Thread(target=monitor_workload, daemon=True)
        monitor.start()
        deadline = time.monotonic() + 120
        while True:
            reply = request("GET", "/", health=True)
            if reply and reply[0] == 200:
                break
            need(time.monotonic() < deadline, "Reference readiness deadline exceeded")
            done.wait(1)
        print(json.dumps({"reference_identity": reply[1]["version"], "image": image_id}), flush=True)
        need(reply[1]["version"]["number"] == manifest["reference"]["server_version"] and
             reply[1]["version"]["build_hash"] == manifest["reference"]["server_revision"], "Actual reference build differs from pinned source")
        index = "hsearch-fixture-" + namespace.rsplit("-", 1)[1]
        status, _ = request("PUT", "/" + index, inputs["schema.json"])
        need(status == 200, "Reference index/schema creation failed")
        documents = inputs["documents.json"]
        ndjson = "".join(json.dumps({"index": {"_index": index, "_id": item["id"]}}) + "\n" + json.dumps(item["source"], ensure_ascii=False) + "\n" for item in documents)
        status, body = request("POST", "/_bulk?refresh=wait_for", ndjson, "application/x-ndjson")
        need(status == 200 and not body["errors"] and len(body["items"]) == len(documents), "Reference dataset load failed")
        for case in inputs["search-cases.json"]:
            query = case["request"]
            status, body = request(query["method"], query["path"].replace("{index}", index), query["json"])
            verify_search(case, status, body)
            print(json.dumps({"reference_case": case["id"], "status": "pass"}), flush=True)
        for case in inputs["analyzer-cases.json"]:
            status, body = request("POST", "/" + index + "/_analyze", {"analyzer": case["analyzer"], "text": case["text"]})
            verify_analyzer(case, status, body)
            print(json.dumps({"reference_analyzer": case["id"], "status": "pass"}), flush=True)
        for case in inputs["mutation-cases.json"]:
            case_index = "hsearch-case-" + namespace.rsplit("-", 1)[1] + "-" + case["id"]
            status, _ = request("PUT", "/" + case_index, inputs["schema.json"])
            need(status == 200, "Reference history index creation failed")
            if case["setup"] == "dataset":
                ndjson = "".join(json.dumps({"index": {"_index": case_index, "_id": item["id"]}}) + "\n" + json.dumps(item["source"], ensure_ascii=False) + "\n" for item in documents)
                status, body = request("POST", "/_bulk?refresh=wait_for", ndjson, "application/x-ndjson")
                need(status == 200 and not body["errors"] and len(body["items"]) == len(documents), "Reference history seed failed")
            bindings = {"index": case_index}
            for number, step in enumerate(case["steps"]):
                query = step["request"]
                path = bind_path(query["path"], bindings)
                body = bind_value(query.get("json"), bindings)
                content_type = "application/json"
                if "ndjson_lines" in query:
                    body = "".join(json.dumps(bind_value(line, bindings), ensure_ascii=False) + "\n" for line in query["ndjson_lines"]) + query.get("raw_tail", "")
                    content_type = "application/x-ndjson"
                status, body = request(query["method"], path, body, content_type)
                verify_step(step["expect"], status, body, bindings)
                for name, field in step.get("capture", {}).items():
                    need(name not in bindings, "Reference capture cannot replace prior authority")
                    bindings[name] = capture_value(pointer(body, field))
                print(json.dumps({"reference_history": case["id"], "step": number, "status": "pass"}), flush=True)
            status, _ = request("DELETE", "/" + case_index)
            need(status == 200, "Reference history index cleanup failed")
        need(not violations and source_identity() == identity and Path(__file__).read_bytes() == controller_bytes,
             "Source, controller or resource state changed during reference execution")
        report = {"source": identity, "controller_sha256": hashlib.sha256(controller_bytes).hexdigest(),
                  "reference_image": image_id, "pinned_manifest": reference, "architecture": architecture,
                  "server_version": reply[1]["version"], "search_cases": len(inputs["search-cases.json"]),
                  "analyzer_cases": len(inputs["analyzer-cases.json"]), "mutation_histories": len(inputs["mutation-cases.json"]),
                  "mutation_steps": integrity["mutation_steps"], "fixture_file_sha256": manifest["files"],
                  "actual_cgroup_limits": actual_limits,
                  "peak_sampled_disk_bytes_including_pull_growth": peak, "observations": results,
                  "network": "none; fixed loopback via owned Docker exec only", "security_profile": "Security plugin disabled for isolated public-toy reference; no credentials or host ports; not hsearch security qualification",
                  "hsearch_conformance": "NOT-RUN", "client_sdk_execution": "NOT-RUN", "establishes_task_completion": False}
        (folder / "observations.json").write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8", newline="\n")
    finally:
        primary = sys.exception()
        done.set()
        if monitor:
            monitor.join(timeout=12)
        try:
            need(not monitor or not monitor.is_alive(), "Reference monitor outcome uncertain; retain lease")
            current = sb.registry.get(namespace)
            if cid:
                info = owned()
                need(not current.get("stop_pending"), "Known stop outcome uncertain; retain lease")
                if info["State"]["Running"]:
                    sb.registry.update(namespace, stop_pending=True)
                    command("kill", cid, limit=4096)
                    sb.registry.update(namespace, stop_pending=False)
                command("wait", cid, timeout=15, limit=4096)
                info = owned()
                need(not info["State"]["Running"] and info["State"]["Status"] in {"created", "exited", "dead"}, "Reference completion uncertain")
                (folder / "final-container-state.json").write_text(json.dumps(info["State"], indent=2) + "\n", encoding="utf-8", newline="\n")
                logs_captured = False
                try:
                    logs = command("logs", "--tail", "100", cid, timeout=10, limit=32768)
                    logs_captured = True
                except Refused as log_error:
                    logs = "Bounded server log unavailable: " + str(log_error)[:256]
                (folder / "server-tail.txt").write_text(logs, encoding="utf-8", newline="\n")
                if primary is None:
                    need(not violations, "Reference resource monitor failed: " + repr(violations))
                    command("container", "rm", cid, limit=4096)
                    need(not command("container", "ls", "--all", "--no-trunc", "--filter", "id=" + cid, "--format", "{{.ID}}", limit=4096).strip(), "Reference removal uncertain")
                else:
                    sb.registry.update(namespace, phase="failed-evidence-retained", owner_active=False, evidence_captured=logs_captured)
                    print(json.dumps({"failed_reference_evidence_retained": cid, "namespace": namespace,
                                      "directory": str(folder), "monitor_errors": violations}), flush=True)
            else:
                need(not current["operation_pending"] and not current["create_pending"], "Reference operation outcome uncertain; retain lease")
            if primary is None or cid is None:
                sb.registry.release(namespace)
                print(json.dumps({"owned_reference_claim_closed": True, "namespace": namespace}), flush=True)
        except Exception as cleanup_error:
            try:
                print(json.dumps({"cleanup_error": str(cleanup_error)[:512], "namespace": namespace}), flush=True)
            except (OSError, ValueError):
                pass
            if primary is None:
                raise
        finally:
            with sb.registry.edit() as data:
                if namespace in data["leases"]:
                    data["leases"][namespace]["owner_active"] = False
    print(json.dumps({"reference_corpus": "pass", "evidence": str(folder), "hsearch_conformance": "NOT-RUN", "client_execution": "NOT-RUN", "establishes_task_completion": False}), flush=True)


if __name__ == "__main__":
    main()
