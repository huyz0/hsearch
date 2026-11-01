"""Run bounded reference fixtures in one owned network-none OpenSearch container."""
from pathlib import Path
import argparse
import tempfile
import hashlib
import json
import re
import runpy
import sys
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
API = runpy.run_path(str(ROOT / "tools/api_fixtures.py"))
SANDBOX = runpy.run_path(str(ROOT / "tools/sandbox.py"))
TASK = runpy.run_path(str(ROOT / "tools/task.py"))
need, Refused = SANDBOX["need"], SANDBOX["Refused"]


def source_identity():
    head = TASK["git"]("rev-parse", "HEAD", root=ROOT).strip()
    state = TASK["source_fingerprint"](ROOT, head, [])
    need(state["clean"], "Reference execution requires actual clean committed source")
    return {"head": head, "tree": TASK["git"]("rev-parse", "HEAD^{tree}", root=ROOT).strip(), "source_digest": state["digest"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-root", type=Path, default=Path(tempfile.gettempdir()) / "hsearch-dev-leases")
    parser.add_argument("--evidence-parent", type=Path, default=Path(tempfile.gettempdir()) / "hsearch-api-reference")
    args = parser.parse_args()
    area = args.evidence_parent.resolve()
    need(not area.is_relative_to(ROOT) and not args.state_root.resolve().is_relative_to(ROOT), "Reference state/evidence must stay outside source")
    controller_bytes = Path(__file__).read_bytes()
    integrity = API["check"]()
    sb = SANDBOX["Sandbox"](args.state_root, source_root=ROOT)
    identity = source_identity()
    dev_image = sb.image()
    namespace = sb.namespace("api-reference")
    owner = "M02.01-reference-" + namespace.rsplit("-", 1)[1]
    folder = area / namespace
    folder.mkdir(parents=True, exist_ok=False)
    fixtures = ROOT / "fixtures/api/core-v1"
    manifest = json.loads((fixtures / "manifest.json").read_text(encoding="utf-8"))
    architecture = sb.docker.info["Architecture"]
    key = {"x86_64": "linux_amd64", "aarch64": "linux_arm64"}.get(architecture)
    need(key is not None, "Unqualified native reference architecture")
    reference = "opensearchproject/opensearch@" + manifest["reference"][key]
    record = {"namespace": namespace, "owner": owner, "source": str(ROOT), "image": dev_image,
              "kind": "api-reference", "phase": "preflight", "container_id": None,
              "operation_pending": True, "create_pending": False, "stop_pending": False, "owner_active": True}
    sb.registry.acquire(namespace, SANDBOX["BUILD"], sb.docker.capacity, sb.docker.engine, record)
    cid, monitor = None, None
    done, violations = threading.Event(), []
    results, peak, pull_growth = [], 0, 0

    def command(*args, timeout=10, limit=131072):
        return sb.docker.command(*args, timeout=timeout, output_bytes=limit)

    def owned():
        info = json.loads(command("container", "inspect", "--size", cid, limit=32768))[0]
        need(info["Id"] == cid and info["Image"] == image_id and
             info["Config"]["Labels"].get(SANDBOX["LABEL"]) == owner and
             info["Config"]["Labels"].get("io.hsearch.namespace") == namespace,
             "Reference identity/ownership changed; preserve resources")
        host = info["HostConfig"]
        need(host["NetworkMode"] == "none" and not host["PortBindings"] and not host["PublishAllPorts"] and
             host["NanoCpus"] == 2000000000 and host["Memory"] == 4 * SANDBOX["GIB"] and
             host["MemorySwap"] == host["Memory"] and host["PidsLimit"] == 512 and
             info["Config"]["User"] == "1000:1000" and
             all(item["Type"] == "tmpfs" for item in info["Mounts"]), "Reference caps/isolation changed")
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
        status, _ = request("PUT", "/" + index, json.loads((fixtures / "schema.json").read_text(encoding="utf-8")))
        need(status == 200, "Reference index/schema creation failed")
        documents = json.loads((fixtures / "documents.json").read_text(encoding="utf-8"))
        ndjson = "".join(json.dumps({"index": {"_index": index, "_id": item["id"]}}) + "\n" + json.dumps(item["source"], ensure_ascii=False) + "\n" for item in documents)
        status, body = request("POST", "/_bulk?refresh=wait_for", ndjson, "application/x-ndjson")
        need(status == 200 and not body["errors"] and len(body["items"]) == len(documents), "Reference dataset load failed")
        for case in json.loads((fixtures / "search-cases.json").read_text(encoding="utf-8")):
            query = case["request"]
            status, body = request(query["method"], query["path"].replace("{index}", index), query["json"])
            need(status == 200, "Reference query failed: " + case["id"])
            oracle = case["reference_oracle"]
            if oracle["kind"] == "hits":
                hits = body["hits"]
                need([item["_id"] for item in hits["hits"]] == oracle["ids"] and hits["total"] == {"value": oracle["exact_total"], "relation": "eq"}, "Reference ID/count/order oracle mismatch: " + case["id"])
                need(all(item["_source"] == {"uid": item["_id"]} for item in hits["hits"]), "Reference source projection mismatch")
            else:
                for name, expected in oracle["values"].items():
                    observed = body["aggregations"][name]
                    if isinstance(expected, list):
                        need([{key: item[key] for key in ("key", "doc_count")} for item in observed["buckets"]] == expected, "Reference bucket oracle mismatch")
                    else:
                        need(observed["value"] == expected, "Reference metric oracle mismatch")
            print(json.dumps({"reference_case": case["id"], "status": "pass"}), flush=True)
        for case in json.loads((fixtures / "analyzer-cases.json").read_text(encoding="utf-8")):
            status, body = request("POST", "/" + index + "/_analyze", {"analyzer": case["analyzer"], "text": case["text"]})
            need(status == 200 and [{key: item[key] for key in ("token", "position", "start_offset", "end_offset")} for item in body["tokens"]] == case["tokens"], "Reference analyzer oracle mismatch: " + case["id"])
            print(json.dumps({"reference_analyzer": case["id"], "status": "pass"}), flush=True)
        need(not violations and source_identity() == identity and Path(__file__).read_bytes() == controller_bytes,
             "Source, controller or resource state changed during reference execution")
        report = {"source": identity, "controller_sha256": hashlib.sha256(controller_bytes).hexdigest(),
                  "reference_image": image_id, "pinned_manifest": reference, "architecture": architecture,
                  "server_version": reply[1]["version"], "search_cases": 13, "analyzer_cases": 4,
                  "peak_sampled_disk_bytes_including_pull_growth": peak, "observations": results,
                  "network": "none; fixed loopback via owned Docker exec only", "security_profile": "Security plugin disabled for isolated public-toy reference; no credentials or host ports; not hsearch security qualification",
                  "hsearch_conformance": "NOT-RUN", "client_sdk_execution": "NOT-RUN", "task_completion": False}
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
                try:
                    logs = command("logs", "--tail", "100", cid, timeout=10, limit=32768)
                except Refused as log_error:
                    logs = "Bounded server log unavailable: " + str(log_error)[:256]
                (folder / "server-tail.txt").write_text(logs, encoding="utf-8", newline="\n")
                if primary is None:
                    need(not violations, "Reference resource monitor failed: " + repr(violations))
                    command("container", "rm", cid, limit=4096)
                    need(not command("container", "ls", "--all", "--no-trunc", "--filter", "id=" + cid, "--format", "{{.ID}}", limit=4096).strip(), "Reference removal uncertain")
                else:
                    sb.registry.update(namespace, phase="failed-evidence-retained", owner_active=False)
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
    print(json.dumps({"reference_search_and_analyzer": "pass", "evidence": str(folder), "hsearch_conformance": "NOT-RUN", "client_execution": "NOT-RUN", "task_completion": False}), flush=True)


if __name__ == "__main__":
    main()
