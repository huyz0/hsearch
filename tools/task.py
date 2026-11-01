#!/usr/bin/env python3
"""Own isolated task worktrees and retain bounded, content-bound progress."""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import runpy
import secrets
import stat
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
SANDBOX = runpy.run_path(str(ROOT / "tools/sandbox.py"))
Refused, need = SANDBOX["Refused"], SANDBOX["need"]
SCHEMA = json.loads((ROOT / "harness/task-packet.schema.json").read_text(encoding="utf-8"))
CLAIM = re.compile(r"m[0-9]{2}-[0-9]{2}-[a-f0-9]{12}\Z")
SHA = re.compile(r"[a-f0-9]{40}\Z")
GIT_OUTPUT_BYTES = 1024**2
SOURCE_FILE_BYTES = 64 * 1024**2
SOURCE_TOTAL_BYTES = 128 * 1024**2
REGISTRY_BYTES = 64 * 1024**2


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def validate_schema(value, schema, location="packet"):
    """Enforce every keyword used by the checked-in packet schema; fail closed."""
    supported = {"$schema", "title", "type", "const", "enum", "additionalProperties", "required", "properties",
                 "pattern", "minLength", "maxLength", "minItems", "maxItems", "uniqueItems", "items", "minimum", "maximum"}
    need(set(schema).issubset(supported), "Unsupported task-schema keyword")
    if "const" in schema:
        need(type(value) is type(schema["const"]) and value == schema["const"], f"Invalid {location} constant")
    if "enum" in schema:
        need(value in schema["enum"], f"Unknown {location} value")
    types = {"object": dict, "array": list, "string": str, "integer": int}
    if "type" in schema:
        need(type(value) is types[schema["type"]], f"Invalid {location} type")
    if isinstance(value, dict):
        need(set(schema.get("required", [])).issubset(value), f"Missing {location} field")
        fields = schema.get("properties", {})
        if schema.get("additionalProperties") is False:
            need(set(value).issubset(fields), f"Unknown {location} field")
        for key, item in value.items():
            if key in fields:
                validate_schema(item, fields[key], f"{location}.{key}")
    if isinstance(value, list):
        need(schema.get("minItems", 0) <= len(value) <= schema.get("maxItems", 100), f"Invalid {location} item count")
        if schema.get("uniqueItems"):
            need(len({encoded(item) for item in value}) == len(value), f"Duplicate {location} item")
        for item in value:
            validate_schema(item, schema["items"], location + "[]")
    if isinstance(value, str):
        need(schema.get("minLength", 0) <= len(value) <= schema.get("maxLength", 4096), f"Invalid {location} length")
        need("\0" not in value, f"NUL in {location}")
        if "pattern" in schema:
            need(re.search(schema["pattern"], value) is not None, f"Invalid {location} shape")
    if type(value) is int:
        need(schema.get("minimum", value) <= value <= schema.get("maximum", value), f"Invalid {location} range")


def relative_path(value):
    need(isinstance(value, str) and value and "\\" not in value and not value.startswith("/"), "Owned path must be relative POSIX")
    parts = value.rstrip("/").split("/")
    need(all(part and part not in {".", ".."} and ":" not in part for part in parts), "Owned path traversal/alias refused")
    need(parts[0].casefold() not in {".git", ".aws", ".codex", ".agents", ".hsearch-local"}, "Reserved task path")
    need(not any(part.casefold().startswith(".env") or part.casefold().endswith((".pem", ".key", ".p12")) for part in parts),
         "Credential-shaped task path refused")
    return "/".join(parts)


def overlaps(first, second):
    a, b = PurePosixPath(first.casefold()).parts, PurePosixPath(second.casefold()).parts
    return a[:len(b)] == b or b[:len(a)] == a


def packet(value):
    validate_schema(value, SCHEMA)
    need(SHA.fullmatch(value["base_revision"]) and re.fullmatch(r"M[0-9]{2}\.[0-9]{2}", value["task_id"]), "Invalid exact task/base identity")
    paths = [relative_path(path) for path in value["owned_paths"]]
    need(all(not overlaps(a, b) for index, a in enumerate(paths) for b in paths[index + 1:]), "Redundant/overlapping owned paths")
    need(len(encoded(value)) <= min(value["output_budget_bytes"], 24000), "Task packet exceeds byte/context budget; narrow it")
    return {**value, "owned_paths": paths}


def git(*args, root=ROOT):
    child = subprocess.Popen(["git", "--no-pager", *args], cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    captured, errors, overflow = {}, [], threading.Event()

    def read(stream, key):
        try:
            captured[key] = stream.read(GIT_OUTPUT_BYTES + 1)
            if len(captured[key]) > GIT_OUTPUT_BYTES:
                overflow.set()
                if child.poll() is None:
                    child.kill()
        except OSError:
            errors.append(key)
            if child.poll() is None:
                child.kill()
        finally:
            stream.close()

    readers = [threading.Thread(target=read, args=(stream, key), daemon=True)
               for stream, key in ((child.stdout, "out"), (child.stderr, "err"))]
    for reader in readers:
        reader.start()
    try:
        child.wait(timeout=60)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait(timeout=3)
        raise Refused("Git command deadline exceeded; inspect owned task state") from None
    finally:
        for reader in readers:
            reader.join(timeout=3)
    need(not any(reader.is_alive() for reader in readers), "Git pipe completion uncertain; inspect owned task state")
    need(not errors, "Git pipe read failed; inspect owned task state")
    need(not overflow.is_set(), "Git metadata output exceeds bound")
    need(child.returncode == 0, f"Git {args[0]} failed; inspect owned task state")
    return captured["out"].decode("utf-8", errors="strict")


def worktrees(root=ROOT):
    result, current = [], None
    for field in git("worktree", "list", "--porcelain", "-z", root=root).split("\0"):
        if field.startswith("worktree "):
            if current:
                result.append(current)
            current = {"path": str(Path(field[9:]).resolve())}
        elif field and current is not None:
            key, _, value = field.partition(" ")
            current[key] = value
    if current:
        result.append(current)
    return result


def ledger_tasks(root=ROOT):
    ledger = json.loads((root / "docs/plan/tasks.json").read_text(encoding="utf-8"))
    return {task["id"]: task for milestone in ledger["milestones"] for task in milestone.get("tasks", [])}


def guarded(root, *parts):
    path = root.joinpath(*parts)
    need(path.resolve() == path.absolute() and path.resolve().is_relative_to(root), "Owned task path is redirected/escaped")
    return path


def handle_live(handle):
    if handle is None:
        return False
    identity = SANDBOX["owner_identity"](handle["pid"])
    return identity == "unknown" or identity is not None and identity == handle["identity"]


class TaskStore:
    def __init__(self, state_root, root=ROOT):
        self.source = Path(root).resolve()
        common = Path(git("rev-parse", "--path-format=absolute", "--git-common-dir", root=self.source).strip()).resolve()
        self.repository = {"id": hashlib.sha256(str(common).encode()).hexdigest()[:20], "common_dir": str(common)}
        state_root = Path(state_root).absolute()
        need(not state_root.is_symlink(), "Task state root cannot be a symlink")
        state_root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.root = state_root.resolve() / self.repository["id"]
        need(not self.root.is_symlink(), "Repository task state cannot be a symlink")
        self.root.mkdir(exist_ok=True, mode=0o700)
        self.path = self.root / "tasks.json"

    @contextmanager
    def edit(self):
        with SANDBOX["file_lock"](self.root / "tasks.lock"):
            need(not self.path.is_symlink(), "Task registry cannot be a symlink")
            need(not self.path.exists() or self.path.stat().st_size <= REGISTRY_BYTES, "Task registry metadata exceeds bound; retain and archive explicitly")
            data = json.loads(self.path.read_text(encoding="utf-8")) if self.path.exists() else {
                "schema_version": 1, "repository": self.repository, "resource_root": None, "tasks": {}}
            need(data["schema_version"] == 1 and data["repository"] == self.repository, "Foreign task registry")
            yield data
            need(len(json.dumps(data, indent=2).encode()) + 1 <= REGISTRY_BYTES, "Task registry metadata exceeds bound; retain and archive explicitly")
            SANDBOX["atomic_json"](self.path, data)

    def get(self, claim, owner=None):
        need(CLAIM.fullmatch(claim), "Invalid claim identifier")
        with self.edit() as data:
            need(claim in data["tasks"], "Unknown task claim")
            record = json.loads(json.dumps(data["tasks"][claim]))
        if owner is not None:
            need(record["packet"]["owner_session"] == owner, "Task belongs to another owner session")
        need(hashlib.sha256(encoded(packet(record["packet"]))).hexdigest() == record["packet_sha256"], "Task packet content changed")
        expected = guarded(self.root, "worktrees", claim)
        need(Path(record["worktree"]) == expected, "Task worktree escaped its allocation")
        return record

    def claim(self, value):
        value = packet(value)
        revision = git("rev-parse", "--verify", value["base_revision"] + "^{commit}", root=self.source).strip()
        need(revision == value["base_revision"], "Task base identity changed")
        ledger = json.loads(git("show", revision + ":docs/plan/tasks.json", root=self.source))
        tasks = {task["id"]: task for milestone in ledger["milestones"] for task in milestone.get("tasks", [])}
        need(value["task_id"] in tasks, "Unknown ledger task")
        task = tasks[value["task_id"]]
        need(task["status"] in {"planned", "in_progress"}, "Ledger task is not claimable")
        need(all(tasks.get(dep, {}).get("status") == "done" for dep in task["depends_on"] if "." in dep),
             "Unfinished task dependency")
        # Milestone prerequisites include M00; verify their actual rollups too.
        milestones = {item["id"]: item for item in ledger["milestones"]}
        need(all(milestones.get(dep, {}).get("status") == "done" for dep in task["depends_on"] if "." not in dep),
             "Unfinished milestone dependency")
        claim = value["task_id"].lower().replace(".", "-") + "-" + secrets.token_hex(6)
        worktree = guarded(self.root, "worktrees", claim)
        worktree.parent.mkdir(exist_ok=True, mode=0o700)
        need(not worktree.exists() and worktree.resolve().is_relative_to(self.root), "Task path already exists/escapes root")
        record = {"packet": value, "packet_sha256": hashlib.sha256(encoded(value)).hexdigest(), "worktree": str(worktree),
                  "branch": "codex/task/" + claim, "phase": "allocating", "namespace": None, "worker": None,
                  "lifecycle_owner": {"pid": os.getpid(), "identity": SANDBOX["owner_identity"]()}, "evidence": []}
        with self.edit() as data:
            need(len(data["tasks"]) < 512, "Task registry claim bound reached; retain and archive explicitly")
            for existing in data["tasks"].values():
                if existing["phase"] == "closed":
                    continue
                need(existing["packet"]["task_id"] != value["task_id"], "Task already claimed")
                need(not any(overlaps(a, b) for a in value["owned_paths"] for b in existing["packet"]["owned_paths"]),
                     "Owned paths overlap another active task")
            data["tasks"][claim] = record
        # On an exception/unknown outcome retain the allocation and path claim.
        git("worktree", "add", str(worktree), "-b", record["branch"], revision, root=self.source)
        with self.edit() as data:
            data["tasks"][claim]["phase"] = "allocated"
        self.append(claim, value["owner_session"], "allocation", {"base": revision, "worktree": str(worktree)})
        return claim

    def append(self, claim, owner, kind, value):
        record = self.get(claim, owner)
        content = encoded(value)
        need(len(content) <= record["packet"]["output_budget_bytes"], "Evidence exceeds task byte budget")
        need(re.fullmatch(r"[a-z][a-z0-9-]{0,31}", kind), "Invalid evidence kind")
        identity = hashlib.sha256(content).hexdigest()
        with self.edit() as data:
            current = data["tasks"][claim]
            need(current["phase"] != "closed", "Closed task evidence is immutable")
            need(len(current["evidence"]) < 1000, "Task evidence record bound exceeded")
            need(sum(item["bytes"] for item in current["evidence"]) + len(content) <= 64 * 1024**2, "Task evidence total byte budget exceeded")
            current["evidence"].append({"kind": kind, "sha256": identity, "bytes": len(content), "time_ns": time.time_ns()})
            need(len(json.dumps(data, indent=2).encode()) + 1 <= REGISTRY_BYTES, "Task registry metadata exceeds bound; retain and archive explicitly")
            folder = guarded(self.root, "evidence", claim)
            folder.mkdir(parents=True, exist_ok=True, mode=0o700)
            path = folder / (identity + ".json")
            stored_count, stored_bytes = 0, 0
            for artifact in folder.iterdir():
                need(not artifact.is_symlink() and artifact.is_file() and re.fullmatch(r"[a-f0-9]{64}\.json", artifact.name),
                     "Unknown evidence artifact; preserve and inspect")
                stored_count += 1
                stored_bytes += artifact.stat().st_size
                need(stored_count <= 1000 and stored_bytes <= 64 * 1024**2, "Evidence artifact storage bound exceeded")
            # Count orphan files from interrupted writes too. Admission and blob
            # publication share the registry lock with cleanup and other appends.
            if path.exists():
                need(path.stat().st_size == len(content) and path.read_bytes() == content, "Evidence digest collision/corruption")
            else:
                need(stored_count < 1000 and stored_bytes + len(content) <= 64 * 1024**2, "Evidence artifact storage bound exceeded")
                with path.open("xb") as target:
                    target.write(content)
                    target.flush()
                    os.fsync(target.fileno())
        return identity

    def evidence(self, claim, owner):
        record = self.get(claim, owner)
        for item in record["evidence"]:
            need(re.fullmatch(r"[0-9a-f]{64}", item["sha256"]), "Invalid evidence reference")
            path = guarded(self.root, "evidence", claim, item["sha256"] + ".json")
            need(not path.is_symlink() and path.stat().st_size == item["bytes"] <= record["packet"]["output_budget_bytes"],
                 "Evidence missing/escaped/over budget")
            content = path.read_bytes()
            need(hashlib.sha256(content).hexdigest() == item["sha256"], "Evidence content changed")
        return record["evidence"]

    def inspect_worktree(self, record):
        path = Path(record["worktree"])
        matches = [item for item in worktrees(self.source) if item["path"] == str(path)]
        need(len(matches) == 1 and matches[0].get("branch") == "refs/heads/" + record["branch"], "Worktree registration/branch changed")
        common = Path(git("rev-parse", "--path-format=absolute", "--git-common-dir", root=path).strip()).resolve()
        need(str(common) == self.repository["common_dir"], "Worktree repository identity changed")
        return path

    def fingerprint(self, record):
        path = self.inspect_worktree(record)
        value = record["packet"]
        flags = git("ls-files", "-v", "-z", root=path).split("\0")
        need(all(not entry or entry.startswith("H ") for entry in flags), "Hidden index flags/unmerged inputs refused")
        ignored = git("ls-files", "--others", "--ignored", "--exclude-standard", "-z", root=path)
        need(not ignored, "Ignored worktree inputs/data present; preserve and inspect before running/cleanup")
        head = git("rev-parse", "HEAD", root=path).strip()

        def tree(revision):
            result = {}
            for entry in filter(None, git("ls-tree", "-r", "-z", revision, root=path).split("\0")):
                meta, name = entry.split("\t", 1)
                mode, kind, oid = meta.split()
                need(kind == "blob" and mode in {"100644", "100755"}, "Unsupported tracked source type")
                result[relative_path(name)] = (mode, oid)
            return result

        base, committed = tree(value["base_revision"]), tree(head)
        index = {}
        for entry in filter(None, git("ls-files", "--stage", "-z", root=path).split("\0")):
            meta, name = entry.split("\t", 1)
            mode, oid, stage = meta.split()
            need(stage == "0" and mode in {"100644", "100755"}, "Unmerged/unsupported task source")
            index[relative_path(name)] = (mode, oid)
        untracked = list(filter(None, git("ls-files", "--others", "--exclude-standard", "-z", root=path).split("\0")))
        names = sorted(set(base) | set(committed) | set(index) | set(untracked))
        need(len(names) <= 10000, "Task source file count exceeds bound")
        manifest, actual, total = [], {}, 0
        for name in names:
            name = relative_path(name)
            candidate = path / name
            component = candidate
            while component != path:
                need(not component.is_symlink(), "Task source path is redirected")
                component = component.parent
            need(candidate.resolve() == candidate.absolute(), "Task source path is redirected")
            if candidate.exists():
                before = candidate.stat()
                need(stat.S_ISREG(before.st_mode) and before.st_size <= SOURCE_FILE_BYTES, "Task input is not a bounded regular file")
                total += before.st_size
                need(total <= SOURCE_TOTAL_BYTES, "Task source aggregate byte bound exceeded")
                blob = hashlib.sha1(f"blob {before.st_size}\0".encode(), usedforsecurity=False)
                content = hashlib.sha256()
                read_bytes = 0
                with candidate.open("rb") as source:
                    opened = os.fstat(source.fileno())
                    need((opened.st_dev, opened.st_ino) == (before.st_dev, before.st_ino), "Task input identity changed")
                    while chunk := source.read(65536):
                        read_bytes += len(chunk)
                        need(read_bytes <= before.st_size, "Task input changed while hashing")
                        blob.update(chunk)
                        content.update(chunk)
                after = candidate.stat()
                need(read_bytes == before.st_size and (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
                     (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns), "Task input changed while hashing")
                mode = index.get(name, ("100644",))[0] if os.name == "nt" else ("100755" if before.st_mode & 0o111 else "100644")
                actual[name] = (mode, blob.hexdigest())
                manifest.append((name, mode, before.st_size, content.hexdigest()))
            if actual.get(name) != base.get(name) or index.get(name) != base.get(name):
                need(any(name.casefold() == owner.casefold() or name.casefold().startswith(owner.casefold() + "/")
                         for owner in value["owned_paths"]), "Worktree changed a path outside its task claim")
        state = {"head": head, "source_tree_sha256": hashlib.sha256(encoded(manifest)).hexdigest(),
                 "index_sha256": hashlib.sha256(encoded(index)).hexdigest(), "input_bytes": total, "file_count": len(actual),
                 "clean": actual == committed and index == committed and not untracked}
        state["digest"] = hashlib.sha256(encoded(state)).hexdigest()
        return state

    def context(self, claim, owner):
        record = self.get(claim, owner)
        references = self.evidence(claim, owner)
        result = {"claim": claim, "phase": record["phase"], "packet": record["packet"],
                  "worktree": record["worktree"], "namespace": record["namespace"],
                  "evidence_count": len(references), "recent_evidence": references[-3:]}
        result.update(context_bytes=0, estimated_tokens_bytes_div4=0, token_count_is_estimate=True)
        for _ in range(5):
            size = len(encoded(result))
            result.update(context_bytes=size, estimated_tokens_bytes_div4=(size + 3) // 4)
        need(len(encoded(result)) <= record["packet"]["output_budget_bytes"], "Context exceeds declared byte budget; narrow packet")
        return result

    def resources(self, resource_root, record):
        canonical = str(Path(resource_root).resolve())
        with self.edit() as data:
            need(data.get("resource_root") in {None, canonical}, "Task coordinator must use its recorded shared resource registry")
            data["resource_root"] = canonical
        return SANDBOX["Sandbox"](resource_root, source_root=Path(record["worktree"]))

    def resume(self, claim, owner, resource_root):
        record = self.get(claim, owner)
        token = secrets.token_hex(12)
        with self.edit() as data:
            current = data["tasks"][claim]
            need(current["phase"] in {"allocated", "running", "interrupted", "resuming"},
                 "Worktree lifecycle outcome needs inspection; allocation retained")
            need(not handle_live(current["worker"]), "Task worker is live or uncertain; no duplicate")
            current.update(phase="resuming", lifecycle_token=token,
                           worker={"pid": os.getpid(), "identity": SANDBOX["owner_identity"]()})
            record = dict(current)
        try:
            self.evidence(claim, owner)
            self.fingerprint(record)
            sandbox = self.resources(resource_root, record)
            with sandbox.registry.edit() as data:
                candidates = [dict(item) for item in data["leases"].values()
                              if item.get("kind") == "topology" and item.get("task_claim") == claim]
            need(len(candidates) <= 1, "Multiple resource namespaces claim this task")
            namespace = record["namespace"]
            if candidates:
                candidate = candidates[0]
                need(candidate["source"] == record["worktree"] and candidate["engine"] == sandbox.docker.engine and
                     candidate["phase"] == "ready", "Resource setup outcome uncertain; preserve namespace")
                need(namespace in {None, candidate["namespace"]}, "Task resource namespace changed")
                namespace = candidate["namespace"]
            else:
                need(namespace is None, "Registered resource lease missing; inspect before resume")
            recovered = sandbox.reconcile(namespace) if namespace else []
            self.append(claim, owner, "resume", {"recovered": recovered, "checks_required": True})
            with self.edit() as data:
                current = data["tasks"][claim]
                need(current["phase"] == "resuming" and current.get("lifecycle_token") == token,
                     "Resume lifecycle ownership changed; preserve state")
                current.update(phase="allocated", namespace=namespace, worker=None)
        except BaseException:
            with self.edit() as data:
                current = data["tasks"][claim]
                if current["phase"] == "resuming" and current.get("lifecycle_token") == token:
                    current.update(phase="interrupted", worker=None)
            raise
        return self.context(claim, owner)

    def run(self, claim, owner, mode, resource_root):
        record = self.get(claim, owner)
        need(mode in record["packet"]["commands"], "Command is outside task packet")
        before = self.fingerprint(record)
        with self.edit() as data:
            current = data["tasks"][claim]
            need(current["phase"] == "allocated" and not handle_live(current["worker"]), "Task worker/lifecycle is busy; resume first")
            current.update(phase="running", worker={"pid": os.getpid(), "identity": SANDBOX["owner_identity"]()}, last_mode=mode)
        completed = False
        try:
            sandbox = self.resources(resource_root, record)
            namespace = record["namespace"]
            if namespace is None:
                namespace = sandbox.up(record["packet"]["task_id"], task_claim=claim)
                with self.edit() as data:
                    data["tasks"][claim]["namespace"] = namespace
            result = sandbox.run(namespace, mode, output_budget=max(4096, record["packet"]["output_budget_bytes"] // 2), stream=False)
            after = self.fingerprint(record)
            need(before["digest"] == after["digest"], "Worktree changed during validation; result is stale")
            self.append(claim, owner, "command", {"source": before, "result": result})
            completed = True
            return {"claim": claim, "mode": mode, "exit_code": result["exit_code"], "source": before}
        except SANDBOX["WorkloadFailed"] as error:
            self.append(claim, owner, "failed-command", {"source": before, "result": error.result})
            raise
        finally:
            with self.edit() as data:
                data["tasks"][claim].update(phase="allocated" if completed else "interrupted", worker=None)

    def cleanup(self, claim, owner, resource_root):
        record = self.get(claim, owner)
        need(record["phase"] == "allocated" and not handle_live(record["worker"]), "Task lifecycle/worker is busy; resume first")
        path = self.inspect_worktree(record)
        need(self.fingerprint(record)["clean"], "Dirty worktree preserved; commit or retain its work")
        need(not git("status", "--porcelain", "-z", "--untracked-files=all", root=path), "Dirty worktree preserved; commit or retain its work")
        self.evidence(claim, owner)
        with self.edit() as data:
            current = data["tasks"][claim]
            need(current["phase"] == "allocated" and not handle_live(current["worker"]), "Task lifecycle became busy")
            current.update(phase="closing", worker={"pid": os.getpid(), "identity": SANDBOX["owner_identity"]()})
        if record["namespace"]:
            sandbox = self.resources(resource_root, record)
            resource = sandbox.registry.get(record["namespace"])
            need(resource.get("task_claim") == claim, "Task resource ownership changed")
            sandbox.down(record["namespace"])
        # Git performs its own clean-worktree check too. No force flag, global
        # pruning, recursive shell deletion, or branch deletion is used.
        git("worktree", "remove", str(path), root=self.source)
        need(not path.exists() and not any(item["path"] == str(path) for item in worktrees(self.source)), "Worktree removal uncertain")
        with self.edit() as data:
            data["tasks"][claim].update(phase="closed", worker=None, namespace=None)
        return {"claim": claim, "phase": "closed", "branch_retained": record["branch"], "evidence_retained": True}


def main():
    shared = argparse.ArgumentParser(add_help=False)
    shared.add_argument("--state-root", type=Path, default=argparse.SUPPRESS)
    shared.add_argument("--resource-state-root", type=Path, default=argparse.SUPPRESS)
    parser = argparse.ArgumentParser(description=__doc__, parents=[shared])
    groups = parser.add_subparsers(dest="group", required=True)
    task = groups.add_parser("task", parents=[shared]).add_subparsers(dest="action", required=True)
    generate = task.add_parser("packet", parents=[shared])
    generate.add_argument("task_id")
    generate.add_argument("--owner", required=True)
    generate.add_argument("--paths", nargs="+", required=True)
    generate.add_argument("--negative", action="append", required=True)
    generate.add_argument("--base")
    generate.add_argument("--output-budget", type=int, default=1024**2)
    claim = task.add_parser("claim", parents=[shared])
    claim.add_argument("packet_file", type=Path)
    task.add_parser("status", parents=[shared])
    for action in ("run", "resume"):
        command = task.add_parser(action, parents=[shared])
        command.add_argument("claim")
        command.add_argument("--owner", required=True)
        if action == "run":
            command.add_argument("mode", choices=("prefetch", "diagnostics", "check", "probe"))
    context = groups.add_parser("context", parents=[shared])
    context.add_argument("claim")
    context.add_argument("--owner", required=True)
    worktree = groups.add_parser("worktree", parents=[shared]).add_subparsers(dest="action", required=True)
    worktree.add_parser("list", parents=[shared])
    close = worktree.add_parser("cleanup", parents=[shared])
    close.add_argument("claim")
    close.add_argument("--owner", required=True)
    evidence = groups.add_parser("evidence", parents=[shared]).add_subparsers(dest="action", required=True)
    for action in ("show", "append"):
        command = evidence.add_parser(action, parents=[shared])
        command.add_argument("claim")
        command.add_argument("--owner", required=True)
        if action == "append":
            command.add_argument("--kind", required=True)
            command.add_argument("--value", required=True)
    args = parser.parse_args()
    args.state_root = getattr(args, "state_root", Path(tempfile.gettempdir()) / "hsearch-tasks")
    args.resource_state_root = getattr(args, "resource_state_root", Path(tempfile.gettempdir()) / "hsearch-dev-leases")
    try:
        store = TaskStore(args.state_root)
        if args.group == "task" and args.action == "packet":
            task = ledger_tasks()[args.task_id]
            result = packet({"schema_version": 1, "task_id": args.task_id, "goal": task["goal"],
                "base_revision": args.base or git("rev-parse", "HEAD").strip(), "owner_session": args.owner,
                "owned_paths": args.paths, "assumptions": ["Existing interfaces and task dependencies remain authoritative."],
                "acceptance": task["definition_of_done"], "negative_cases": args.negative,
                "commands": ["prefetch", "diagnostics", "check", "probe"], "output_budget_bytes": args.output_budget,
                "expected_evidence": ["Exact source identity, bounded check results and independent review before integration."]})
        elif args.group == "task" and args.action == "claim":
            need(args.packet_file.stat().st_size <= 1024**2, "Packet file exceeds bound")
            result = {"claim": store.claim(json.loads(args.packet_file.read_text(encoding="utf-8")))}
        elif args.group == "task" and args.action == "status" or args.group == "worktree" and args.action == "list":
            with store.edit() as data:
                result = {key: {"task": value["packet"]["task_id"], "phase": value["phase"], "worktree": value["worktree"]}
                          for key, value in data["tasks"].items()}
        elif args.group == "context":
            result = store.context(args.claim, args.owner)
        elif args.group == "task" and args.action == "resume":
            result = store.resume(args.claim, args.owner, args.resource_state_root)
        elif args.group == "task" and args.action == "run":
            result = store.run(args.claim, args.owner, args.mode, args.resource_state_root)
        elif args.group == "worktree":
            result = store.cleanup(args.claim, args.owner, args.resource_state_root)
        elif args.action == "append":
            need(len(args.value.encode()) <= 1024**2, "Evidence input exceeds bound")
            result = {"sha256": store.append(args.claim, args.owner, args.kind, json.loads(args.value))}
        else:
            result = store.evidence(args.claim, args.owner)
        print(encoded(result).decode())
        return 0
    except (Refused, OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(f"REFUSED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
