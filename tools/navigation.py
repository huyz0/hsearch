#!/usr/bin/env python3
"""Own one capped, source-bound Linux semantic session per task worktree."""
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import time

ROOT = Path(__file__).resolve().parents[1]


class Navigation:
    def __init__(self, store, claim, owner, resources, source):
        self.store, self.claim, self.owner, self.source = store, claim, owner, source
        self.task = store.get(claim, owner)
        self.api = type(store).__init__.__globals__
        self.need, self.Refused = self.api["need"], self.api["Refused"]
        self.resources = resources
        self.helpers = self.api["SANDBOX"]

    def connect(self):
        self.sandbox = self.store.resources(self.resources, self.task)
        self.registry, self.docker = self.sandbox.registry, self.sandbox.docker

    def existing(self):
        with self.registry.edit() as data:
            candidates = [dict(item) for item in data["leases"].values()
                          if item.get("kind") == "navigation" and item.get("task_claim") == self.claim]
        self.need(len(candidates) <= 1, "Multiple semantic namespaces claim this task")
        if not candidates:
            return None
        value = candidates[0]
        self.need(value["source"] == self.task["worktree"] and value["owner_session"] == self.owner and
                  value["engine"] == self.docker.engine, "Semantic session ownership changed")
        return value

    @contextmanager
    def exclusive(self):
        token = secrets.token_hex(12)
        with self.store.edit() as data:
            current = data["tasks"][self.claim]
            self.need(current["phase"] == "allocated" and not self.api["handle_live"](current["worker"]),
                      "Task is busy; resume an interrupted task before semantic work")
            current.update(phase="navigating", lifecycle_token=token,
                           worker={"pid": os.getpid(), "identity": self.helpers["owner_identity"]()})
        complete = False
        try:
            yield
            complete = True
        finally:
            with self.store.edit() as data:
                current = data["tasks"][self.claim]
                self.need(current["phase"] == "navigating" and current.get("lifecycle_token") == token,
                          "Semantic lifecycle ownership changed")
                current.update(phase="allocated" if complete else "interrupted", worker=None)

    def image(self):
        digest = self.helpers["image_digest"]()
        path = self.registry.root / "images" / (digest + ".json")
        self.need(path.is_file(), "Pinned navigation image is unavailable; run sandbox image")
        value = json.loads(path.read_text(encoding="utf-8"))
        self.need(value["engine"] == self.docker.engine and value["build_inputs_sha256"] == digest and
                  re.fullmatch(r"[a-f0-9]{64}", value["navigation_artifact"]["binary_sha256"]), "Pinned navigation image is stale")
        return value

    def inspect(self, record, cid):
        return self.sandbox.run_container(record, cid)

    def create(self, record, name, command, *, network):
        namespace = record["namespace"]
        self.verify_volumes(record)
        args = ["create", "--name", name, "--label", f"{self.helpers['LABEL']}={record['owner']}",
                "--label", f"io.hsearch.namespace={namespace}", "--log-driver", "local", "--log-opt", "max-size=16m",
                "--log-opt", "max-file=1", "--log-opt", "compress=false", "--user", "10001:10001", "--read-only",
                "--cpus", "2", "--memory", str(4 * 1024**3), "--memory-swap", str(4 * 1024**3), "--pids-limit", "512",
                "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--network", network,
                "--tmpfs", "/tmp:rw,nosuid,nodev,size=67108864", "--env", "CARGO_NET_OFFLINE=true" if network == "none" else "CARGO_NET_OFFLINE=false",
                "--env", "CARGO_TARGET_DIR=/target/navigation/analysis", "--env", "LSP_CLI_HOME=/target/navigation/state",
                "--env", f"HSEARCH_EXPECTED_LSP_SHA256={record['lsp_sha256']}",
                "--env", f"HSEARCH_SOURCE_DIGEST={record['source_digest']}",
                "--env", f"HSEARCH_BACKEND_RESERVE_BYTES={record['reserve_bytes']}",
                "--mount", f"type=bind,source={record['source']},target=/workspace,readonly",
                "--mount", f"type=bind,source={record['harness']},target=/harness,readonly",
                "--mount", f"type=volume,source={namespace}-cargo,target=/cargo",
                "--mount", f"type=volume,source={namespace}-target,target=/target", record["image"], *command]
        cid = self.docker.command(*args).strip()
        self.need(re.fullmatch(r"[a-f0-9]{64}", cid), "Unacknowledged semantic container create; retain lease")
        role = "prefetch_container_id" if name.endswith("-prefetch") else "container_id"
        self.registry.update(namespace, **{role: cid, "create_pending": False})
        self.inspect(record, cid)
        self.verify_volumes(record)
        return cid

    def start(self):
        existing = self.existing()
        if existing:
            if existing["source_digest"] == self.source["digest"] and existing["phase"] == "ready" and not existing.get("query_pending"):
                info = self.inspect(existing, existing["container_id"])
                if info["State"]["Running"]:
                    return existing
            self.close(existing)
        image = self.image()
        namespace, owner = self.sandbox.namespace("navigation"), secrets.token_hex(16)
        record = {"kind": "navigation", "protocol": 3, "namespace": namespace, "owner": owner,
                  "source": self.task["worktree"], "owner_session": self.owner, "task_claim": self.claim,
                  "image": image["id"], "lsp_sha256": image["navigation_artifact"]["binary_sha256"],
                  "source_digest": self.source["digest"], "phase": "preflight", "query_pending": None,
                  "prefetch_container_id": None, "container_id": None, "create_pending": False}
        self.registry.acquire(namespace, self.helpers["BUILD"], self.docker.capacity, self.docker.engine, record)
        free = self.sandbox.backend_free(record, namespace + "-preflight")
        self.sandbox.require_backend_reserve(free)
        with self.registry.edit() as data:
            reserve = sum(item["budget"]["disk_bytes"] for item in data["leases"].values()) + 10 * 1024**3
        record["reserve_bytes"] = reserve
        self.registry.update(namespace, reserve_bytes=reserve)
        self.registry.update(namespace, preflight_complete=True, phase="volumes")
        record.update(preflight_complete=True)
        for suffix in ("cargo", "target"):
            name = namespace + "-" + suffix
            self.docker.command("volume", "create", "--label", f"{self.helpers['LABEL']}={owner}",
                                "--label", f"io.hsearch.namespace={namespace}", name)
            self.helpers["assert_owned"](self.docker.inspect("volume", [name]), owner)
        folder = self.registry.root / "navigation" / namespace
        self.need(folder.resolve() == folder.absolute() and not folder.exists(), "Semantic snapshot path redirected/reused")
        folder.mkdir(parents=True, mode=0o700)
        harness = folder / "harness"
        harness.mkdir(mode=0o755)
        harness.chmod(0o755)
        hashes = {}
        for name in ("navigation.py", "execute.py"):
            content = (ROOT / "dev" / name).read_bytes()
            self.need(len(content) < 65536, "Semantic controller exceeds source bound")
            (harness / name).write_bytes(content)
            (harness / name).chmod(0o444)
            hashes[name] = hashlib.sha256(content).hexdigest()
        record.update(harness=str(harness), harness_hashes=hashes)
        self.registry.update(namespace, harness=str(harness), harness_hashes=hashes, volumes_complete=True,
                             phase="prefetch_creating", create_pending=True)
        cid = self.create(record, namespace + "-prefetch", ["python3", "/harness/execute.py", "prefetch", "--disk-bytes",
                          str(8 * 1024**3), "--timeout", "1200", "--output-bytes", str(1024**2)], network="bridge")
        self.registry.update(namespace, prefetch_container_id=cid, create_pending=False, phase="prefetch")
        self.docker.command("start", cid)
        status = self.docker.command("wait", cid, timeout=1250).strip()
        info = self.inspect(record, cid)
        self.need(status.isdigit() and not info["State"]["Running"] and info["State"]["ExitCode"] == int(status), "Semantic prefetch outcome uncertain")
        self.docker.command("container", "rm", cid)
        self.registry.update(namespace, prefetch_container_id=None, phase="prefetched")
        self.need(int(status) == 0, "Semantic dependency prefetch failed; close the owned session before retry")
        with self.registry.edit() as data:
            reserve = sum(item["budget"]["disk_bytes"] for item in data["leases"].values()) + 10 * 1024**3
        record["reserve_bytes"] = reserve
        self.registry.update(namespace, reserve_bytes=reserve)
        self.registry.update(namespace, phase="service_creating", create_pending=True)
        cid = self.create(record, namespace + "-server", ["python3", "/harness/navigation.py", "serve", "--reserve-bytes", str(reserve)], network="none")
        self.registry.update(namespace, container_id=cid, create_pending=False, phase="starting")
        self.docker.command("start", cid)
        deadline = time.monotonic() + 60
        while True:
            info = self.inspect(record, cid)
            self.need(info["State"]["Running"], "Semantic controller exited before readiness; close owned resources")
            health = json.loads(self.docker.command("exec", cid, "python3", "/harness/navigation.py", "health", timeout=30))
            if health.get("ready") is True:
                self.need(health["source_digest"] == self.source["digest"] and health["identity"]["lsp_sha256"] == record["lsp_sha256"],
                          "Semantic readiness identity mismatch")
                self.registry.update(namespace, phase="ready", identity=health["identity"])
                return self.registry.get(namespace)
            self.need(time.monotonic() < deadline, "Semantic controller readiness deadline; retain owned resources")
            time.sleep(0.1)

    def verify_snapshot(self, record):
        folder = Path(record["harness"])
        self.need(folder.resolve() == folder.absolute() and set(item.name for item in folder.iterdir()) == set(record["harness_hashes"]),
                  "Semantic controller snapshot changed")
        for name, digest in record["harness_hashes"].items():
            script = folder / name
            self.need(not script.is_symlink() and script.stat().st_size < 65536 and hashlib.sha256(script.read_bytes()).hexdigest() == digest,
                      "Semantic controller snapshot changed")
        self.verify_volumes(record)

    def verify_volumes(self, record):
        names = [record["namespace"] + "-" + suffix for suffix in ("cargo", "target")]
        volumes = self.docker.inspect("volume", names)
        self.need(len(volumes) == 2 and {item["Name"] for item in volumes} == set(names), "Semantic cache volume identity changed")
        self.helpers["assert_owned"](volumes, record["owner"])
        self.need(all(item["Labels"].get("io.hsearch.namespace") == record["namespace"] for item in volumes), "Semantic cache namespace changed")

    def query(self, value):
        record = self.existing()
        if record is None:
            record = self.start()
        if record["source_digest"] != self.source["digest"]:
            return {"status": "unavailable", "reason": "stale_semantic_session; start a fresh source-bound session", "semantic": False}
        self.need(record["phase"] == "ready" and not record.get("query_pending"), "Semantic query outcome is pending/uncertain; close session first")
        self.verify_snapshot(record)
        self.need(self.inspect(record, record["container_id"])["State"]["Running"], "Semantic session expired; start a new session")
        token = secrets.token_hex(12)
        self.registry.update(record["namespace"], query_pending=token)
        with self.registry.edit() as data:
            reserve = sum(item["budget"]["disk_bytes"] for item in data["leases"].values()) + 10 * 1024**3
        raw = self.docker.command("exec", record["container_id"], "python3", "/harness/navigation.py", "query",
                                  json.dumps({**value, "source_digest": self.source["digest"], "reserve_bytes": reserve}, separators=(",", ":")), timeout=140)
        self.need(len(raw.encode()) <= 131073, "Semantic response exceeded frozen controller bound")
        result = json.loads(raw)
        with self.registry.edit() as data:
            current = data["leases"][record["namespace"]]
            self.need(current.get("query_pending") == token, "Semantic query ownership changed")
            current["query_pending"] = None
        return result

    def close(self, record=None):
        record = record or self.existing()
        if record is None:
            return {"status": "stopped", "data": {"already_stopped": True}}
        self.need(record.get("preflight_complete") and record.get("volumes_complete") and not record.get("create_pending"),
                  "Semantic setup outcome uncertain; retain namespace for inspection")
        for cid in (record.get("prefetch_container_id"), record.get("container_id")):
            if not cid:
                continue
            visible = self.docker.command("container", "ls", "--all", "--no-trunc", "--filter", f"id={cid}", "--format", "{{.ID}}").split()
            if not visible:
                continue
            self.need(visible == [cid], "Semantic container identity changed")
            info = self.inspect(record, cid)
            if info["State"]["Running"]:
                self.docker.command("kill", "--signal", "TERM", cid)
                self.docker.command("wait", cid, timeout=15)
                self.need(not self.inspect(record, cid)["State"]["Running"], "Semantic controller still running; no forced cleanup")
            self.docker.command("container", "rm", cid)
        remaining = self.docker.command("container", "ls", "--all", "--filter", f"label=io.hsearch.namespace={record['namespace']}", "--format", "{{.ID}}").strip()
        self.need(not remaining, "Unacknowledged semantic container remains; lease retained")
        expected = {record["namespace"] + "-" + suffix for suffix in ("cargo", "target")}
        names = self.docker.command("volume", "ls", "--filter", f"name=^{record['namespace']}-(cargo|target)$", "--format", "{{.Name}}").split()
        all_owned = self.docker.command("volume", "ls", "--filter", f"label=io.hsearch.namespace={record['namespace']}", "--format", "{{.Name}}").split()
        self.need(set(names).issubset(expected) and set(all_owned).issubset(expected), "Unexpected semantic volume; lease retained")
        info = self.docker.inspect("volume", names)
        self.helpers["assert_owned"](info, record["owner"])
        self.need(all(item["Labels"].get("io.hsearch.namespace") == record["namespace"] for item in info), "Semantic volume namespace changed")
        if names:
            self.docker.command("volume", "rm", *names)
        self.need(not self.docker.command("volume", "ls", "--filter", f"name=^{record['namespace']}-(cargo|target)$", "--format", "{{.Name}}").strip(),
                  "Semantic volume removal uncertain; lease retained")
        self.registry.release(record["namespace"])
        return {"status": "stopped", "data": {"namespace": record["namespace"], "known_ids_removed_without_force": True}}


def navigate(store, claim, owner, value, resources, source):
    refused = type(store).__init__.__globals__["Refused"]
    try:
        navigation = Navigation(store, claim, owner, resources, source)
        with navigation.exclusive():
            navigation.connect()
            if value["operation"] == "stop":
                return navigation.close()
            if value["operation"] == "start":
                record = navigation.start()
                return {"status": "ready", "data": {"namespace": record["namespace"], "source_digest": record["source_digest"], "identity": record["identity"]}}
            return navigation.query(value)
    except (refused, OSError, ValueError, KeyError, TypeError) as error:
        return {"status": "unavailable", "semantic": False, "reason": str(error)[:256]}
