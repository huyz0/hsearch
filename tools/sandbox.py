#!/usr/bin/env python3
"""Owned, capped local Linux development resources. No production or deployment commands."""
from __future__ import annotations
import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
from string import Template

ROOT = Path(__file__).resolve().parents[1]
GIB = 1024**3
LABEL = "io.hsearch.owner"
NAMESPACE = re.compile(r"hsearch-[a-z0-9-]{1,25}-[0-9a-f]{12}\Z")
VOLUMES = ("ca", "server", "client", "credentials", "data", "cargo", "target")
BUILD = {"cpu_millis": 2000, "memory_bytes": 4 * GIB, "pids": 512, "disk_bytes": 8 * GIB}
STORAGE = {"cpu_millis": 1000, "memory_bytes": 2 * GIB, "pids": 256, "disk_bytes": 10 * GIB}
PROBE = {"cpu_millis": 500, "memory_bytes": 256 * 1024**2, "pids": 64, "disk_bytes": 1024**2}


class Refused(RuntimeError):
    """An unsafe, unavailable or over-budget operation was refused."""


def need(condition, message):
    if not condition:
        raise Refused(message)


def owner_identity(pid=None):
    pid = os.getpid() if pid is None else pid
    if os.name == "nt":
        import ctypes
        from ctypes import wintypes
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        kernel.OpenProcess.restype = wintypes.HANDLE
        kernel.GetProcessTimes.argtypes = [wintypes.HANDLE, *([ctypes.POINTER(wintypes.FILETIME)] * 4)]
        kernel.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
        kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        handle = kernel.OpenProcess(0x1000 | 0x100000, False, pid)
        if not handle:
            return None if ctypes.get_last_error() == 87 else "unknown"
        try:
            values = [wintypes.FILETIME() for _ in range(4)]
            if not kernel.GetProcessTimes(handle, *(ctypes.byref(v) for v in values)):
                return "unknown"
            if kernel.WaitForSingleObject(handle, 0) == 0:
                return None
            return f"windows:{values[0].dwHighDateTime}:{values[0].dwLowDateTime}"
        finally:
            kernel.CloseHandle(handle)
    try:
        stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        if stat[0] == "Z":
            return None
        return Path("/proc/sys/kernel/random/boot_id").read_text().strip() + ":" + stat[19]
    except FileNotFoundError:
        return None
    except OSError:
        return "unknown"


@contextmanager
def file_lock(path, timeout=10):
    need(not path.is_symlink(), "Registry lock cannot be a symlink")
    with path.open("a+b") as handle:
        if handle.seek(0, 2) == 0:
            handle.write(b"0")
            handle.flush()
        deadline = time.monotonic() + timeout
        while True:
            try:
                handle.seek(0)
                if os.name == "nt":
                    import msvcrt
                    msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
                else:
                    import fcntl
                    fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except OSError:
                need(time.monotonic() < deadline, "Host registry lock busy; no resources changed")
                time.sleep(0.05)
        try:
            yield
        finally:
            handle.seek(0)
            if os.name == "nt":
                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(handle, fcntl.LOCK_UN)


def atomic_json(path, data):
    need(not path.is_symlink(), "Registry file cannot be a symlink")
    fd, temporary = tempfile.mkstemp(prefix=".registry-", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
            json.dump(data, handle, indent=2)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def admit(request, leases, capacity, free_disk):
    need(set(request) == set(BUILD), "Unknown resource budget shape")
    for key, value in request.items():
        need(type(value) is int and value > 0, f"Invalid {key} budget")
        total = value + sum(lease["budget"][key] for lease in leases)
        need(total <= capacity[key], f"Host {key} budget exhausted")
    need(free_disk >= request["disk_bytes"] + 10 * GIB, "Low host disk reserve; no resources changed")


class Registry:
    def __init__(self, root, free_disk=None):
        self.root = Path(root).absolute()
        need(not self.root.is_symlink(), "Registry root cannot be a symlink")
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.root = self.root.resolve()
        self.path = self.root / "registry.json"
        self.free_disk = free_disk or (lambda: shutil.disk_usage(self.root).free)

    @contextmanager
    def edit(self):
        with file_lock(self.root / "registry.lock"):
            need(not self.path.is_symlink(), "Registry file cannot be a symlink")
            data = json.loads(self.path.read_text()) if self.path.exists() else {"schema_version": 1, "leases": {}}
            need(data.get("schema_version") == 1 and isinstance(data.get("leases"), dict), "Invalid host registry")
            yield data
            atomic_json(self.path, data)

    def acquire(self, namespace, budget, capacity, engine, record=None):
        with self.edit() as data:
            need(namespace not in data["leases"], "Lease already exists")
            need(all(x["engine"] == engine for x in data["leases"].values()), "Registry belongs to a different engine")
            admit(budget, data["leases"].values(), capacity, self.free_disk())
            lease = {"budget": budget, "engine": engine, "pid": os.getpid(), "process_identity": owner_identity(), **(record or {})}
            data["leases"][namespace] = lease
            return lease

    def get(self, namespace):
        with self.edit() as data:
            need(namespace in data["leases"], "Unknown resource lease")
            return data["leases"][namespace]

    def update(self, namespace, **changes):
        with self.edit() as data:
            need(namespace in data["leases"], "Unknown resource lease")
            data["leases"][namespace].update(changes)

    def release(self, namespace):
        with self.edit() as data:
            need(namespace in data["leases"], "Unknown resource lease")
            del data["leases"][namespace]


class Docker:
    def __init__(self):
        self.context = self.command("context", "show").strip()
        context = json.loads(self.command("context", "inspect", self.context))[0]
        need(context["Endpoints"]["docker"]["Host"].startswith(("unix://", "npipe://")), "Only a local Docker engine is allowed")
        self.info = json.loads(self.command("info", "--format", "{{json .}}"))
        need(self.info["OSType"] == "linux" and self.info["CgroupVersion"] == "2", "Linux cgroup-v2 Docker engine required")
        self.engine = self.info["ID"]
        self.capacity = {"cpu_millis": min(8000, max(1000, (self.info["NCPU"] - 1) * 1000)),
                         "memory_bytes": min(16 * GIB, self.info["MemTotal"] // 2), "pids": 2048, "disk_bytes": 40 * GIB}

    def command(self, *args, stream=False, timeout=180, input_text=None):
        prefix = ["docker"] + (["--context", self.context] if hasattr(self, "context") else [])
        try:
            result = subprocess.run([*prefix, *args], text=True, input=input_text, capture_output=not stream, timeout=timeout)
        except (OSError, subprocess.TimeoutExpired) as error:
            raise Refused(f"Docker unavailable or operation outcome requires inspection: {type(error).__name__}") from error
        need(result.returncode == 0, f"Docker operation failed ({args[0]}); inspect owned resources before retry")
        return "" if stream else result.stdout

    def inspect(self, kind, names):
        return json.loads(self.command(kind, "inspect", *names)) if names else []


def owned_snapshot(record):
    namespace = record["namespace"]
    need(NAMESPACE.fullmatch(namespace), "Invalid resource namespace")
    folder = ROOT / ".hsearch-local" / namespace
    need(folder.resolve().is_relative_to(ROOT.resolve()) and not folder.is_symlink(), "Owned path escapes worktree")
    return folder


def assert_owned(objects, owner):
    for item in objects:
        labels = item.get("Labels") if "Labels" in item else item.get("Config", {}).get("Labels")
        need(labels and labels.get(LABEL) == owner, "Cleanup refused: resource owner does not match")


class Sandbox:
    def __init__(self, state_root, docker=None, free_disk=None):
        self.registry = Registry(state_root, free_disk=free_disk)
        self.docker = Docker() if docker is None else docker
        self.images = json.loads((ROOT / "dev/images.json").read_text())

    def namespace(self, task):
        need(re.fullmatch(r"[A-Za-z0-9.-]{1,25}", task), "Invalid task identifier")
        return "hsearch-" + task.lower().replace(".", "-") + "-" + secrets.token_hex(6)

    def image(self):
        lease_id = self.namespace("image")
        self.registry.acquire(lease_id, BUILD, self.docker.capacity, self.docker.engine, {"kind": "image"})
        complete = False
        try:
            source = (ROOT / "dev/Dockerfile").read_bytes()
            need(source.decode().splitlines()[0] == "FROM " + self.images["rust"], "Development base image pin disagrees with inventory")
            digest = hashlib.sha256(source).hexdigest()
            tag = "hsearch-dev:" + digest[:20]
            self.docker.command("build", "--pull", "--file", "-", "--tag", tag, str(ROOT / "dev"),
                                input_text=source.decode(), stream=True, timeout=600)
            info = json.loads(self.docker.command("image", "inspect", tag))[0]
            need(info["Config"]["User"] == "10001:10001", "Development image must be non-root")
            (self.registry.root / "images").mkdir(exist_ok=True, mode=0o700)
            atomic_json(self.registry.root / "images" / (digest + ".json"), {"id": info["Id"], "dockerfile_sha256": digest, "engine": self.docker.engine})
            self.docker.command("pull", self.images["rustfs"], stream=True, timeout=300)
            complete = True
            return info["Id"]
        finally:
            if complete:
                self.registry.release(lease_id)
            else:
                self.registry.update(lease_id, phase="uncertain-image-operation")

    def up(self, task):
        digest = hashlib.sha256((ROOT / "dev/Dockerfile").read_bytes()).hexdigest()
        image = json.loads((self.registry.root / "images" / (digest + ".json")).read_text())
        need(image["engine"] == self.docker.engine, "Development image belongs to another engine")
        need(image["dockerfile_sha256"] == hashlib.sha256((ROOT / "dev/Dockerfile").read_bytes()).hexdigest(), "Development image is stale")
        namespace = self.namespace(task)
        owner = secrets.token_hex(16)
        record = {"kind": "topology", "namespace": namespace, "owner": owner, "source": str(ROOT), "image": image["id"], "phase": "starting"}
        self.registry.acquire(namespace, STORAGE, self.docker.capacity, self.docker.engine, record)
        folder = owned_snapshot(record)
        prepared, backend_read_complete = False, False
        try:
            backend_output = self.docker.command("run", "--rm", "--name", namespace + "-preflight", "--label", f"{LABEL}={owner}",
                               "--label", f"io.hsearch.namespace={namespace}", "--read-only", "--network", "none", "--user", "10001:10001",
                               "--cpus", "0.25", "--memory", "67108864", "--pids-limit", "32", "--cap-drop", "ALL",
                               "--security-opt", "no-new-privileges", image["id"], "python3", "-c",
                               "import shutil; print(shutil.disk_usage('/').free)")
            backend_read_complete = True
            backend_free = int(backend_output)
            need(backend_free >= STORAGE["disk_bytes"] + 10 * GIB, "Low Docker backend disk reserve; topology not started")
            folder.mkdir(parents=True, exist_ok=False)
            template = Template((ROOT / "dev/compose.json").read_text())
            # JSON-escape substituted path values before parsing the concrete snapshot.
            values = {"DEV_IMAGE": image["id"], "RUSTFS_IMAGE": self.images["rustfs"], "NAMESPACE": namespace,
                      "OWNER": owner, "SOURCE_ROOT": str(ROOT).replace("\\", "/")}
            config = json.loads(template.substitute({key: json.dumps(value)[1:-1] for key, value in values.items()}))
            path = folder / "compose.json"
            atomic_json(path, config)
            self.registry.update(namespace, config_sha256=hashlib.sha256(path.read_bytes()).hexdigest())
            prepared = True
            for suffix in VOLUMES:
                self.docker.command("volume", "create", "--label", f"{LABEL}={owner}", "--label", f"io.hsearch.namespace={namespace}", f"{namespace}-{suffix}")
                assert_owned(self.docker.inspect("volume", [f"{namespace}-{suffix}"]), owner)
            self.compose(record, "up", "--detach", "--wait", "--wait-timeout", "100")
            containers = self.docker.command("container", "ls", "--all", "--filter", f"label=com.docker.compose.project={namespace}", "--format", "{{.ID}}").split()
            info = self.docker.inspect("container", containers)
            assert_owned(info, owner)
            storage = next(item for item in info if item["Config"]["Labels"].get("com.docker.compose.service") == "rustfs")
            need(not any(storage["NetworkSettings"]["Ports"].values()) and not storage["HostConfig"]["PortBindings"],
                 "Private storage must not expose a host port")
            network = storage["NetworkSettings"]["Networks"][namespace + "-net"]
            self.registry.update(namespace, phase="ready", containers=[x["Id"] for x in info],
                                 network=network["NetworkID"], private_address=network["IPAddress"], endpoint="https://rustfs:9000")
            return namespace
        except BaseException:
            if backend_read_complete and not prepared:
                self.registry.release(namespace)
            else:
                self.registry.update(namespace, phase="failed")
            raise

    def compose(self, record, *args):
        path = owned_snapshot(record) / "compose.json"
        current = self.registry.get(record["namespace"])
        need(hashlib.sha256(path.read_bytes()).hexdigest() == current["config_sha256"], "Owned Compose snapshot changed")
        return self.docker.command("compose", "--project-name", record["namespace"], "--file", str(path), *args, timeout=150)

    def run(self, namespace, mode):
        record = self.registry.get(namespace)
        need(record.get("source") == str(ROOT) and record["engine"] == self.docker.engine and record["phase"] == "ready", "Namespace unavailable in this worktree/engine")
        lease_id = self.namespace("run")
        budget = PROBE if mode == "probe" else BUILD
        with self.registry.edit() as data:
            need(data["leases"][namespace]["phase"] == "ready", "Namespace lifecycle is busy")
            need(not any(x.get("parent") == namespace for x in data["leases"].values()), "Mutable cache/target is already leased")
            admit(budget, data["leases"].values(), self.docker.capacity, self.registry.free_disk())
            data["leases"][lease_id] = {"kind": "run", "parent": namespace, "engine": self.docker.engine, "budget": budget,
                                         "pid": os.getpid(), "process_identity": owner_identity()}
        container = namespace + "-" + lease_id[-12:]
        args = ["run", "--rm", "--name", container, "--label", f"{LABEL}={record['owner']}", "--label", f"io.hsearch.namespace={namespace}",
                "--user", "10001:10001", "--cpus", str(budget["cpu_millis"] / 1000), "--memory", str(budget["memory_bytes"]), "--pids-limit", str(budget["pids"]), "--read-only",
                "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--tmpfs", "/tmp:rw,nosuid,nodev,size=67108864",
                "--network", "bridge" if mode == "prefetch" else (namespace + "-net" if mode == "probe" else "none"),
                "--mount", f"type=bind,source={ROOT},target=/workspace,readonly", "--mount", f"type=volume,source={namespace}-cargo,target=/cargo",
                "--mount", f"type=volume,source={namespace}-target,target=/target"]
        if mode == "probe":
            args += ["--mount", f"type=volume,source={namespace}-client,target=/certs,readonly",
                     "--mount", f"type=volume,source={namespace}-credentials,target=/run/secrets,readonly", record["image"], "python3", "dev/probe.py", namespace]
        else:
            args += [record["image"], "python3", "dev/execute.py", mode, "--disk-bytes", str(BUILD["disk_bytes"]), "--timeout", "1200"]
        try:
            self.docker.command(*args, stream=True, timeout=1250)
        finally:
            # A timeout/interrupt can leave the container alive. Keep its lease then.
            live = self.docker.command("container", "ls", "--all", "--filter", f"name=^/{container}$", "--format", "{{.ID}}").split()
            if not live:
                self.registry.release(lease_id)

    def down(self, namespace):
        record = self.registry.get(namespace)
        need(record.get("source") == str(ROOT) and record["engine"] == self.docker.engine, "Cleanup belongs to another worktree/engine")
        with self.registry.edit() as data:
            need(not any(x.get("parent") == namespace for x in data["leases"].values()), "Cleanup refused: active or uncertain run lease")
            current = data["leases"][namespace]
            need(current["phase"] in {"starting", "ready", "failed", "stopping"}, "Unknown namespace lifecycle")
            if current["phase"] in {"starting", "stopping"}:
                identity = owner_identity(current["pid"])
                need(identity is None or (identity != "unknown" and identity != current["process_identity"]),
                     "Cleanup refused: active or uncertain lifecycle owner")
            current.update(phase="stopping", pid=os.getpid(), process_identity=owner_identity())
        containers = self.docker.command("container", "ls", "--all", "--filter", f"label=com.docker.compose.project={namespace}", "--format", "{{.ID}}").split()
        all_owned = self.docker.command("container", "ls", "--all", "--filter", f"label=io.hsearch.namespace={namespace}", "--format", "{{.ID}}").split()
        need(set(containers) == set(all_owned), "Cleanup refused: unexpected standalone container")
        info = self.docker.inspect("container", containers)
        assert_owned(info, record["owner"])
        if record.get("containers"):
            need({x["Id"] for x in info}.issubset(record["containers"]), "Cleanup refused: container identity changed")
        volumes = self.docker.command("volume", "ls", "--filter", f"name=^{namespace}-", "--format", "{{.Name}}").split()
        expected_volumes = {f"{namespace}-{name}" for name in VOLUMES}
        need(set(volumes).issubset(expected_volumes), "Cleanup refused: unexpected volume")
        assert_owned(self.docker.inspect("volume", volumes), record["owner"])
        networks = self.docker.command("network", "ls", "--filter", f"name=^{namespace}-net$", "--format", "{{.ID}}").split()
        assert_owned(self.docker.inspect("network", networks), record["owner"])
        self.compose(record, "down", "--volumes")
        # Unused declared volumes may not be removed by Compose; remove exact owned names only.
        remaining = self.docker.command("volume", "ls", "--filter", f"label=io.hsearch.namespace={namespace}", "--format", "{{.Name}}").split()
        need(set(remaining).issubset(volumes), "Cleanup refused: unexpected volume")
        if remaining:
            assert_owned(self.docker.inspect("volume", remaining), record["owner"])
            self.docker.command("volume", "rm", *remaining)
        for kind, field in (("container", "{{.ID}}"), ("volume", "{{.Name}}"), ("network", "{{.ID}}")):
            flags = ("--all",) if kind == "container" else ()
            need(not self.docker.command(kind, "ls", *flags, "--filter", f"label=io.hsearch.namespace={namespace}", "--format", field).strip(),
                 "Cleanup incomplete; resource lease retained")
        self.registry.release(namespace)
        # Keep the small owned snapshot/evidence; never recursively delete host source.

    def smoke(self):
        namespaces = []
        try:
            first = self.up("smoke-a")
            namespaces.append(first)
            for mode in ("prefetch", "diagnostics", "check", "probe"):
                self.run(first, mode)
            second = self.up("smoke-b")
            namespaces.append(second)
            self.run(second, "probe")
            first_info, second_info = self.registry.get(first), self.registry.get(second)
            need(first_info["network"] != second_info["network"] and first_info["private_address"] != second_info["private_address"],
                 "Private serving namespaces collided")
            self.down(first)
            namespaces.remove(first)
            self.run(second, "probe")
            print(json.dumps({"smoke": "pass", "isolated_private_ports": True, "published_host_ports": False,
                              "isolated_buckets": True, "other_topology_survived_cleanup": True}))
        finally:
            for namespace in reversed(namespaces):
                self.down(namespace)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-root", type=Path, default=Path(tempfile.gettempdir()) / "hsearch-dev-leases")
    parser.add_argument("command", choices=("image", "up", "run", "down", "status", "smoke"))
    parser.add_argument("target", nargs="?")
    parser.add_argument("mode", nargs="?", choices=("prefetch", "check", "probe", "diagnostics"))
    args = parser.parse_args()
    try:
        sandbox = Sandbox(args.state_root)
        if args.command == "image":
            print(json.dumps({"image": sandbox.image()}))
        elif args.command == "up":
            print(json.dumps({"namespace": sandbox.up(args.target or "dev")}))
        elif args.command == "run":
            need(args.target and args.mode, "run requires namespace and mode")
            sandbox.run(args.target, args.mode)
        elif args.command == "down":
            need(args.target, "down requires namespace")
            sandbox.down(args.target)
        elif args.command == "smoke":
            sandbox.smoke()
        else:
            with sandbox.registry.edit() as data:
                print(json.dumps(data, indent=2))
        return 0
    except (Refused, OSError, ValueError, KeyError) as error:
        print(f"REFUSED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
