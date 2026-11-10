#!/usr/bin/env python3
"""Fixed direct-image capability workload. Not the existing generic smoke.

The owner controller supplies authenticated plan/phase snapshots and restart
receipt. This process has no Docker socket, compiler, credential output or retry.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys
import threading
import time
from xml.sax.saxutils import escape

from rustfs_probe_history import Recorder, WIRE_BYTES, canonical, plan_wire, validate_plan
from rustfs_probe_transport import Refused as TransportRefused, Transport, bounded_xml, digest_payload, token, need


class Client:
    """Execute only the independently frozen finite matrix, once per phase."""

    def __init__(self, value, transport, *, recorder=None):
        self.plan = validate_plan(value)
        self.transport = transport
        self.recorder = recorder or Recorder(value)
        self.results = {}
        self.uploads = {}
        self.parts = {}
        self.versions = {}
        self.cancel = threading.Event()

    def restore(self, raw):
        self.recorder.restore(raw)
        for event in self.recorder.events:
            self.results[event["id"]] = event
            spec = self.plan["operations"][event["id"]-1]
            if spec["action"] == "init" and event["auxiliary"]:
                self.uploads[spec["id"]] = event["auxiliary"]
            if spec["action"] == "part" and event["etag"]:
                self.parts[(spec["parent"], spec["offset"])] = event["etag"]
            if spec["action"] == "versions":
                self.versions[spec["id"]] = json.loads(event["auxiliary"])

    def call(self, spec, barrier=None):
        attempt = self.recorder.reserve(spec)
        binding = self.plan["binding"]
        request = binding["run"] + "-" + str(spec["id"])
        condition = selected = auxiliary = ""
        parent = self.results.get(spec["parent"], {})
        action = spec["action"]
        if action in ("resolve", "replay", "reuse-id"):
            request = binding["run"]+"-"+str(spec["parent"])
        headers = {}
        method, query, body = "GET", [], None
        key = binding["run"] + "/" + spec["key"]
        length = spec["length"]
        result = None
        diagnostic_error = ""
        try:
            if action == "restart":
                raise ValueError("host-hook-required")
            if action in ("update", "range", "replay", "stale-update"):
                need(parent.get("status") in (200, 201, 204) and parent.get("boundary") == "reply",
                     "parent-receipt-unavailable")
            if action == "bucket":
                method, key = "PUT", ""
            elif action == "versioning":
                method, key, query = "PUT", "", [("versioning", "")]
                body = b'<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>'
                length = len(body)
            elif action in ("create", "update", "replay", "stale-update", "reuse-id"):
                method = "PUT"
                condition = "*" if action in ("create", "reuse-id") else parent.get("etag", "")
                need(bool(condition), "condition-unavailable")
                if action == "reuse-id":
                    request = parent["request"]
                if action == "replay":
                    original = self.plan["operations"][spec["parent"]-1]
                    condition = self.results[spec["parent"]]["condition"]
                    request = self.results[spec["parent"]]["request"]
                headers["if-none-match" if action in ("create", "reuse-id") else "if-match"] = condition
                headers["x-amz-meta-request"] = request
            elif action == "head":
                method, length = "HEAD", 0
            elif action in ("race-head", "race-range"):
                winners = [event for ident, event in self.results.items()
                           if self.plan["operations"][ident-1]["race"] == spec["offset"]
                           and event["status"] in (200, 201, 204) and event["boundary"] == "reply"]
                need(len(winners) == 1, "race-winner-not-unique")
                if action == "race-head":
                    method = "HEAD"
                else:
                    selected = winners[0]["version"]
                    need(selected and selected != "null", "exact-version-unavailable")
                    query = [("versionId", selected)]
                    headers["range"] = "bytes=0-"+str(spec["range_length"]-1)
            elif action == "range":
                selected = parent.get("version", "")
                need(bool(selected) and selected != "null", "exact-version-unavailable")
                query = [("versionId", selected)]
                headers["range"] = "bytes=" + str(spec["offset"]) + "-" + str(spec["offset"]+spec["range_length"]-1)
            elif action == "versions":
                method, key, query = "GET", "", [("versions", ""), ("prefix", key), ("max-keys", "16")]
            elif action == "resolve-head":
                versions = self.versions.get(spec["parent"], [])
                if spec["offset"] >= len(versions):
                    return self.record(spec, attempt, self.empty("not-applicable"), request)
                method, selected = "HEAD", versions[spec["offset"]]
                query = [("versionId", selected)]
            elif action == "resolve":
                original = self.plan["operations"][spec["parent"]-1]
                condition = parent.get("condition", "")
                original_request = binding["run"]+"-"+str(original["id"])
                candidates = [event for ident, event in self.results.items()
                              if self.plan["operations"][ident-1]["action"] == "resolve-head"
                              and self.plan["operations"][ident-1]["key"] == spec["key"]
                              and event["status"] == 200 and event["metadata"].split("|", 1)[0] == original_request]
                if not candidates and original["action"] == "complete":
                    # Explicit negative experiment: inspect the one init-bound
                    # historical object, but never call it completion resolution.
                    initiation = binding["run"]+"-"+str(original["parent"])
                    candidates = [event for ident,event in self.results.items()
                                  if self.plan["operations"][ident-1]["action"] == "resolve-head"
                                  and self.plan["operations"][ident-1]["key"] == spec["key"]
                                  and event["status"] == 200 and event["metadata"].split("|",1)[0] == initiation]
                    diagnostic_error = "completion-envelope-not-provider-bound"
                need(len(candidates) == 1, "original-version-not-uniquely-resolved")
                selected = candidates[0]["version"]
                need(selected and selected != "null", "exact-version-unavailable")
                query = [("versionId", selected)]
                request = binding["run"]+"-"+str(original["id"])
                condition = parent.get("condition", "")
                auxiliary = str(candidates[0]["id"])
            elif action == "init":
                method, query = "POST", [("uploads", "")]
                headers["x-amz-meta-request"] = request
            else:
                upload = self.uploads.get(spec["parent"], "")
                need(bool(upload), "upload-identity-unavailable")
                query = [("uploadId", upload)]
                if action == "part":
                    method = "PUT"
                    query += [("partNumber", str(spec["offset"]))]
                elif action in ("complete", "complete-invalid"):
                    method = "POST"
                    condition = "*"
                    headers["if-none-match"] = condition
                    parts = sorted((number, etag) for (owner, number), etag in self.parts.items()
                                   if owner == spec["parent"])
                    need(0 < len(parts) <= 16, "completion-parts")
                    if action == "complete-invalid":
                        parts = list(reversed(parts)) if spec["offset"] == 1 else [parts[0], *parts]
                    body = ("<CompleteMultipartUpload>" + "".join(
                        "<Part><PartNumber>"+str(number)+"</PartNumber><ETag>"+escape(etag)+"</ETag></Part>"
                        for number, etag in parts) + "</CompleteMultipartUpload>").encode()
                    length = len(body)
                    auxiliary = ";".join(str(number)+":"+etag.encode().hex() for number, etag in parts)
                    headers["content-type"] = "application/xml"
                elif action == "abort":
                    method = "DELETE"
                elif action == "uploads":
                    key, query = "", [("uploads", ""), ("prefix", binding["run"]+"/")]
                elif action != "parts":
                    raise ValueError("unknown-action")
            if action in ("create", "update", "replay", "stale-update", "reuse-id", "init", "complete", "complete-invalid", "part"):
                envelope = {"body_sha256": hashlib.sha256(body).hexdigest() if body is not None else digest_payload(spec["seed"], length),
                            "bucket": binding["bucket"], "caller_metadata": "probe-caller-v2", "condition": condition,
                            "key": key, "operation": request, "parts": auxiliary,
                            "upload": self.uploads.get(spec["parent"], "")}
                headers.update({"x-amz-meta-request": request, "x-amz-meta-envelope": hashlib.sha256(canonical(envelope)).hexdigest(),
                                "x-amz-meta-caller": "probe-caller-v2"})
            result = self.transport.attempt(method, binding["bucket"], key, query, headers,
                                             body=body, seed=spec["seed"], length=length,
                                             response_limit=(spec["range_length"] if action in ("range", "race-range") else
                                                             16*1024**2 if action == "resolve" else 16384),
                                             barrier=barrier, suppress=spec["fault"] == "suppress",
                                             disconnect=spec["fault"] == "disconnect", cancel=self.cancel)
            # A complete multipart API can report an error inside HTTP200.
            # Parse only bounded XML; never relay its foreign error text.
            if action in ("init", "complete", "parts", "uploads", "versions") and result["status"] < 300:
                fields = bounded_xml(result["body"])
                if action in ("parts", "uploads", "versions"):
                    need([v for n,v in fields if n == "IsTruncated"] == ["false"], "list-result-truncated")
                need(not any(name == "Error" for name, _ in fields), "embedded-provider-error")
                if action == "init":
                    values = [value for name, value in fields if name == "UploadId"]
                    need(len(values) == 1 and 0 < len(values[0].encode()) <= 256, "upload-id")
                    auxiliary = values[0]
                    token(auxiliary)
                    self.uploads[spec["id"]] = auxiliary
                elif action == "complete":
                    etags = [value for name, value in fields if name == "ETag"]
                    need(len(etags) == 1, "completion-etag")
                    result["etag"] = token(etags[0])
                elif action == "versions":
                    need(not any(name == "IsTruncated" and value != "false" for name, value in fields), "version-list-truncated")
                    keys = [value for name, value in fields if name == "Key"]
                    versions = [value for name, value in fields if name == "VersionId"]
                    need(len(versions) <= 16 and len(keys) == len(versions) and
                         all(k == binding["run"]+"/"+spec["key"] for k in keys) and
                         len(set(versions)) == len(versions) and all(v and v != "null" and len(v.encode()) <= 256 for v in versions), "version-list-shape")
                    auxiliary = canonical(versions).decode()
                    for version in versions:
                        token(version)
                    self.versions[spec["id"]] = versions
                else:
                    wanted = {"PartNumber", "ETag", "Size"} if action == "parts" else {"Key", "UploadId"}
                    for name, value in fields:
                        if name in wanted:
                            token(value, 1024 if name == "Key" else 256)
                    auxiliary = ";".join(name+":"+value.encode().hex() for name, value in fields if name in wanted)
                    need(len(auxiliary.encode()) <= 1024, "list-result-bound")
            if action == "part" and result["status"] == 200 and result["etag"]:
                self.parts[(spec["parent"], spec["offset"])] = result["etag"]
            if diagnostic_error and not result.get("error"):
                result["error"] = diagnostic_error
        except (ValueError, RuntimeError) as error:
            code = str(error)
            # Only local stable codes can be serialized. Never arbitrary errors.
            if (not isinstance(error, TransportRefused) and code != "unknown-action") or not code.replace("-", "").isalnum() or len(code) > 64:
                code = "capture-refused"
            if result is None:
                result = self.empty("not-transmitted", code)
            else:
                result["error"] = code
        return self.record(spec, attempt, result, request, condition, selected, auxiliary)

    def record(self, spec, attempt, result, request, condition="", selected="", auxiliary=""):
        event = self.recorder.record(spec, attempt, result, request=request, condition=condition,
                                     selected=selected, auxiliary=auxiliary)
        self.results[spec["id"]] = event
        return event

    @staticmethod
    def empty(boundary, code=""):
        return {"boundary": boundary, "status": 0, "request_bytes": 0, "response_bytes": 0, "response_wire_bytes": 0,
                "digest": hashlib.sha256(b"").hexdigest(), "error": code}

    def run(self, phase, *, restart_receipt=None):
        need(phase in ("before-restart", "after-restart"), "phase")
        rows = self.plan["operations"]
        marker = next(row["id"] for row in rows if row["action"] == "restart")
        need((phase == "before-restart" and not self.results) or
             (phase == "after-restart" and set(self.results) == set(range(1, marker))), "phase-state-binding")
        while len(self.results) < len(rows):
            spec = rows[len(self.results)]
            if spec["action"] == "restart":
                if phase == "before-restart":
                    return self.recorder.wire(complete=False)
                need(type(restart_receipt) is dict, "restart-receipt-unavailable")
                auxiliary = canonical(restart_receipt).decode()
                need(len(auxiliary.encode()) <= 1024, "restart-receipt-bound")
                attempt = self.recorder.reserve(spec)
                self.record(spec, attempt, self.empty("host-restart"),
                            self.plan["binding"]["run"]+"-"+str(spec["id"]), auxiliary=auxiliary)
            elif spec["race"]:
                group = [r for r in rows if r["race"] == spec["race"]]
                need(len(group) == 16 and all(r["id"] not in self.results for r in group), "race-membership")
                barrier = threading.Barrier(16)
                errors = []

                def worker(row):
                    try:
                        self.call(row, barrier)
                    except BaseException:
                        self.cancel.set()
                        barrier.abort()
                        errors.append("race-capture-failed")

                threads = [threading.Thread(target=worker, args=(r,), daemon=True) for r in group]
                for thread in threads:
                    thread.start()
                for thread in threads:
                    thread.join(16)
                need(not errors and all(not t.is_alive() for t in threads), "race-capture-incomplete")
            else:
                self.call(spec)
        return self.recorder.wire(complete=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("before-restart", "after-restart"))
    args = parser.parse_args()
    # Fixed authenticated input mounts; never accept arbitrary paths/commands.
    def bounded_file(path, cap):
        path = Path(path)
        need(path.resolve() == path.absolute() and not path.is_symlink() and path.is_file() and path.stat().st_size <= cap,
             "fixed-input-bound")
        with path.open("rb") as source:
            raw = source.read(cap+1)
        need(len(raw) <= cap, "fixed-input-bound")
        return raw
    config = bounded_file("/probe-input/config.json", 65536)
    need(len(config) <= 65536, "configuration-bound")
    config = json.loads(config)
    value = validate_plan(config["plan"])
    need(hashlib.sha256(plan_wire(value)).hexdigest() == config["plan_sha256"], "plan-digest")
    def credential(name):
        path = Path("/run/secrets") / name
        need(path.stat().st_size <= 1024, "credential-bound")
        with path.open("rb") as source:
            raw = source.read(1025)
        need(len(raw) <= 1024, "credential-bound")
        return raw.decode().strip()
    transport = Transport("https://rustfs:9000", "/certs/ca.crt", credential("access"), credential("secret"))
    client = Client(value, transport, recorder=Recorder(value, clock_origin=config["clock_origin"]))
    if args.phase == "after-restart":
        prior = bounded_file("/probe-input/prior.history", WIRE_BYTES)
        need(len(prior) <= WIRE_BYTES, "prior-bound")
        need(hashlib.sha256(prior).hexdigest() == config["prior_sha256"], "prior-digest")
        client.restore(prior)
    # Flush every intent before its send and every completion as observed.
    # The host retains bounded Docker logs/charges even if this process dies.
    initial = client.recorder.wire(complete=False)
    sys.stdout.buffer.write(initial[:initial.rfind(b"END\t")])
    sys.stdout.buffer.flush()

    def emit(raw):
        sys.stdout.buffer.write(raw)
        sys.stdout.buffer.flush()

    client.recorder.emit = emit
    raw = client.run(args.phase, restart_receipt=config.get("restart"))
    sys.stdout.buffer.write(raw[raw.rfind(b"END\t"):])
    sys.stdout.buffer.flush()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, RuntimeError, KeyError):
        print("rustfs-capability-capture-refused", file=sys.stderr)
        raise SystemExit(2) from None
