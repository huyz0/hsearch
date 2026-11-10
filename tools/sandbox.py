#!/usr/bin/env python3
"""Owned, capped local Linux development resources. No production or deployment commands."""
from __future__ import annotations
import argparse
from contextlib import contextmanager
import hashlib
import json
import runpy
import os
from pathlib import Path
import re
import runpy
import secrets
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from string import Template

ROOT = Path(__file__).resolve().parents[1]
GIB = 1024**3
LABEL = "io.hsearch.owner"
NAMESPACE = re.compile(r"hsearch-[a-z0-9-]{1,25}-[0-9a-f]{12}\Z")
VOLUMES = ("ca", "server", "client", "credentials", "data", "cargo", "target")
HARNESS_FILES = ("execute.py", "init.py", "probe.py", "install_navigation.py", "install_correctness.py", "coverage_entry.py", "compile_entry.py")
OBJECT = {"cpu_millis": 2000, "memory_bytes": 4 * 1024**3, "pids": 256, "disk_bytes": 18 * 1024**3}
PROBE_HARNESS = ("rustfs_probe.py", "rustfs_probe_transport.py", "rustfs_probe_history.py")
PROBE_CLIENT = {"rustfs-capabilities-before", "rustfs-capabilities-after"}
PROBE_MODES = PROBE_CLIENT | {"probe-checker-build", "probe-history-check"}
BUILD = {"cpu_millis": 2000, "memory_bytes": 4 * GIB, "pids": 256, "disk_bytes": 8 * GIB}
NAVIGATION = {"cpu_millis": 1000, "memory_bytes": 2 * GIB, "pids": 256, "disk_bytes": 4 * GIB}
STORAGE = {"cpu_millis": 1000, "memory_bytes": 2 * GIB, "pids": 256, "disk_bytes": 10 * GIB}
PROBE = {"cpu_millis": 500, "memory_bytes": 256 * 1024**2, "pids": 64, "disk_bytes": 1024**2}


class Refused(RuntimeError):
    """An unsafe, unavailable or over-budget operation was refused."""


class WorkloadFailed(Refused):
    """A known completed workload failed; its bounded result remains reviewable."""

    def __init__(self, result):
        self.result = result
        super().__init__(f"Workload failed: exit={result['exit_code']}, truncated={result['truncated']}")


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
    leases = list(leases)
    need(set(request) == set(BUILD), "Unknown resource budget shape")
    for key, value in request.items():
        need(type(value) is int and value > 0, f"Invalid {key} budget")
        total = value + sum(lease["budget"][key] for lease in leases)
        need(total <= capacity[key], f"Host {key} budget exhausted")
    reserved_disk = request["disk_bytes"] + sum(lease["budget"]["disk_bytes"] for lease in leases)
    need(free_disk >= reserved_disk + 10 * GIB, "Low host disk reserve; no resources changed")


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


def bounded_capture(command, *, timeout, limit, raw_streams=False, capture_status=False):
    """Bound both host pipes before decoding untrusted guest output."""
    child = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    captured, failed, overflow = {}, [], threading.Event()

    def read(stream, key):
        try:
            captured[key] = stream.read(limit + 1)
            if len(captured[key]) > limit:
                overflow.set()
                if child.poll() is None:
                    child.kill()
        except OSError:
            failed.append(key)
            if child.poll() is None:
                child.kill()
        finally:
            stream.close()

    readers = [threading.Thread(target=read, args=(pipe, name), daemon=True)
               for pipe, name in ((child.stdout, "out"), (child.stderr, "err"))]
    for reader in readers:
        reader.start()
    try:
        child.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait(timeout=3)
        raise Refused("Docker exec deadline; operation outcome requires inspection") from None
    finally:
        for reader in readers:
            reader.join(timeout=3)
    need(not any(reader.is_alive() for reader in readers) and not failed, "Docker exec pipe completion uncertain")
    need(not overflow.is_set() and sum(map(len, captured.values())) <= limit, "Docker exec output exceeds bound; inspect owned resources")
    if capture_status:
        need(raw_streams, "Status capture requires raw streams")
        return {"exit_code": child.returncode, "streams": {"stdout": captured["out"], "stderr": captured["err"]}}
    need(child.returncode == 0, "Docker exec failed; operation outcome requires inspection")
    return {"stdout": captured["out"], "stderr": captured["err"]} if raw_streams else captured["out"].decode("utf-8", errors="strict")


class Docker:
    def __init__(self):
        self.context = self.command("context", "show").strip()
        context = json.loads(self.command("context", "inspect", self.context))[0]
        need(context["Endpoints"]["docker"]["Host"].startswith(("unix://", "npipe://")), "Only a local Docker engine is allowed")
        self.info = json.loads(self.command("info", "--format", "{{json .}}"))
        need(self.info["OSType"] == "linux" and self.info["CgroupVersion"] == "2", "Linux cgroup-v2 Docker engine required")
        need(self.info.get("CgroupDriver") in {"systemd", "cgroupfs"} and
             all(self.info.get(flag) is True for flag in ("MemoryLimit", "SwapLimit", "CpuCfsPeriod", "CpuCfsQuota", "PidsLimit")),
             "Docker resource controllers unavailable; hard caps cannot be enforced")
        self.engine = self.info["ID"]
        self.capacity = {"cpu_millis": min(8000, max(1000, (self.info["NCPU"] - 1) * 1000)),
                         "memory_bytes": min(16 * GIB, self.info["MemTotal"] // 2), "pids": 2048, "disk_bytes": 40 * GIB}

    def command(self, *args, stream=False, timeout=180, input_text=None, merge_output=False, output_bytes=None, raw_streams=False, capture_status=False):
        need(not raw_streams or output_bytes is not None, "Raw Docker output requires a bounded capture")
        need(not capture_status or raw_streams, "Docker status capture requires raw pipes")
        prefix = ["docker"] + (["--context", self.context] if hasattr(self, "context") else [])
        try:
            if output_bytes is not None:
                need(not stream and input_text is None and not merge_output and 0 < output_bytes <= 1024**2,
                     "Invalid bounded Docker exec profile")
                return bounded_capture([*prefix, *args], timeout=timeout, limit=output_bytes, raw_streams=raw_streams, capture_status=capture_status)
            options = {} if stream else ({"stdout": subprocess.PIPE, "stderr": subprocess.STDOUT} if merge_output else {"capture_output": True})
            result = subprocess.run([*prefix, *args], text=True, errors="replace", input=input_text, timeout=timeout, **options)
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


def image_digest():
    inputs = {name: hashlib.sha256((ROOT / "dev" / name).read_bytes()).hexdigest()
              for name in ("Dockerfile", ".dockerignore", "navigation-tools.json", "correctness-tools.json", "install_navigation.py", "install_correctness.py", "coverage_entry.py", "execute.py", "compile_entry.py")}
    inputs["assembly_controller"] = hashlib.sha256((ROOT / "tools/sandbox.py").read_bytes()).hexdigest()
    return hashlib.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()


def freeze_harness(destination):
    """Publish public controller code for a different, non-root guest UID."""
    destination.mkdir(mode=0o755)
    # The host registry's ancestors stay private. The bind-mounted directory
    # itself must be traversable by UID 10001 even under a host umask of 077.
    destination.chmod(0o755)
    hashes = {}
    for name in (*HARNESS_FILES, *PROBE_HARNESS):
        content = (ROOT / "dev" / name).read_bytes()
        need(len(content) < 65536, "Trusted harness script exceeds bound")
        script = destination / name
        with script.open("xb") as handle:
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
        script.chmod(0o444)
        hashes[name] = hashlib.sha256(content).hexdigest()
    return hashes


class Sandbox:
    def __init__(self, state_root, docker=None, free_disk=None, *, source_root=None):
        self.registry = Registry(state_root, free_disk=free_disk)
        self.docker = Docker() if docker is None else docker
        self.source_root = ROOT if source_root is None else Path(source_root).resolve()
        need(self.source_root.is_dir(), "Sandbox source root is unavailable")
        self.images = json.loads((ROOT / "dev/images.json").read_text())

    def snapshot(self, record):
        if record.get("snapshot_schema") != 2:
            need(self.source_root == ROOT, "Legacy snapshot cannot adopt a worker worktree")
            return owned_snapshot(record)
        need(NAMESPACE.fullmatch(record["namespace"]), "Invalid resource namespace")
        path = self.registry.root / "snapshots" / record["namespace"]
        need(path.resolve() == path.absolute(), "Sandbox snapshot path is redirected")
        return path

    def harness(self, record):
        if record.get("snapshot_schema") != 2:
            need(self.source_root == ROOT, "Legacy harness cannot adopt a worker worktree")
            return ROOT / "dev"
        path = self.snapshot(record) / "harness"
        declared = self.registry.get(record["namespace"])["harness_hashes"]
        roster = set(declared)
        need(roster in (set(HARNESS_FILES), set(HARNESS_FILES) | set(PROBE_HARNESS)) and
             path.resolve() == path.absolute() and set(item.name for item in path.iterdir()) == roster,
             "Frozen harness path/content changed")
        hashes = {}
        for name in sorted(roster):
            script = path / name
            need(not script.is_symlink() and script.stat().st_size < 65536, "Frozen harness script changed")
            hashes[name] = hashlib.sha256(script.read_bytes()).hexdigest()
        need(hashes == self.registry.get(record["namespace"])["harness_hashes"], "Frozen harness script changed")
        return path

    def namespace(self, task):
        need(re.fullmatch(r"[A-Za-z0-9.-]{1,25}", task), "Invalid task identifier")
        return "hsearch-" + task.lower().replace(".", "-") + "-" + secrets.token_hex(6)

    def backend_free(self, record, name, *, data_volumes=False):
        mounts = []
        paths = ["/"]
        if data_volumes:
            for suffix, target in (("cargo", "/cargo"), ("target", "/target"), ("data", "/data")):
                mounts += ["--mount", f"type=volume,source={record['namespace']}-{suffix},target={target},readonly,volume-nocopy"]
                paths.append(target)
        script = "import shutil; print(min(shutil.disk_usage(p).free for p in " + repr(paths) + "))"
        output = self.docker.command("run", "--rm", "--name", name, "--label", f"{LABEL}={record['owner']}",
                 "--label", f"io.hsearch.namespace={record['namespace']}", "--read-only", "--network", "none", "--user", "10001:10001",
                 "--cpus", "0.25", "--memory", "67108864", "--memory-swap", "67108864", "--pids-limit", "32", "--cap-drop", "ALL",
                 "--security-opt", "no-new-privileges", *mounts, record["image"], "python3", "-c", script)
        return int(output)

    def require_backend_reserve(self, free):
        with self.registry.edit() as data:
            reserved = sum(lease["budget"]["disk_bytes"] for lease in data["leases"].values())
            need(free >= reserved + 10 * GIB, "Low Docker backend disk reserve; workload not started")

    def image(self):
        digest = image_digest()
        cached_path = self.registry.root / "images" / (digest + ".json")
        if cached_path.is_file():
            cached = json.loads(cached_path.read_text(encoding="utf-8"))
            need(cached["engine"] == self.docker.engine and cached["build_inputs_sha256"] == digest,
                 "Cached development image belongs to another engine/source")
            try:
                info = json.loads(self.docker.command("image", "inspect", cached["id"]))[0]
            except Refused:
                info = None
            if info is not None:
                need(info["Id"] == cached["id"] and info["Config"]["User"] == "10001:10001", "Cached development image identity changed")
                return cached["id"]
        lease_id = self.namespace("image")
        self.registry.acquire(lease_id, BUILD, self.docker.capacity, self.docker.engine, {"kind": "image"})
        complete = False
        try:
            source = (ROOT / "dev/Dockerfile").read_bytes()
            need(source.decode().splitlines()[0] == "FROM " + self.images["rust"], "Development base image pin disagrees with inventory")
            digest = image_digest()
            image_folder = self.registry.root / "images"
            image_folder.mkdir(exist_ok=True, mode=0o700)
            base_iid = image_folder / (lease_id + ".base.iid")
            base_tag = "hsearch-nav-base:" + digest[:12] + "-" + lease_id[-12:]
            self.docker.command("build", "--pull", "--file", "-", "--tag", base_tag, "--iidfile", str(base_iid), str(ROOT / "dev"),
                                input_text=source.decode(), stream=True, timeout=600)
            base_id = base_iid.read_text().strip()
            need(re.fullmatch(r"sha256:[a-f0-9]{64}", base_id), "Base image identity uncertain")
            base = json.loads(self.docker.command("image", "inspect", base_id))[0]
            need(base["Config"]["User"] == "10001:10001", "Development base must be non-root")
            self.registry.update(lease_id, bootstrap_image_id=base_id, build_inputs_sha256=digest)
            output = self.build_navigation(lease_id, base_id)
            need(output.resolve() == output.absolute() and {item.name for item in output.iterdir()} == {"lsp", "attestation.json", "cargo-nextest", "correctness-attestation.json"},
                 "Unexpected navigation build artifact")
            for name in ("lsp", "attestation.json", "cargo-nextest", "correctness-attestation.json"):
                artifact = output / name
                need(not artifact.is_symlink() and artifact.is_file(), "Navigation build artifact redirected")
            need((output / "lsp").stat().st_size <= 64 * 1024**2 and (output / "attestation.json").stat().st_size < 4096,
                 "Navigation artifact exceeds bounds")
            binary_hash = hashlib.sha256()
            with (output / "lsp").open("rb") as binary:
                while chunk := binary.read(65536):
                    binary_hash.update(chunk)
            artifact = json.loads((output / "attestation.json").read_text())
            profile = json.loads((ROOT / "dev/navigation-tools.json").read_text(encoding="utf-8"))
            profile_hash = hashlib.sha256(json.dumps(profile, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
            need(artifact["revision"] == profile["lsp_cli"]["revision"] and artifact["version"] == "lsp " + profile["lsp_cli"]["version"] and
                 artifact["cargo_lock_sha256"] == profile["lsp_cli"]["cargo_lock_sha256"] and artifact["profile_sha256"] == profile_hash and
                 artifact["binary_sha256"] == binary_hash.hexdigest() and artifact["rustc"].startswith("rustc " + profile["rust_analyzer"]["toolchain"] + " "),
                 "Navigation build identity mismatch")
            correctness_profile = json.loads((ROOT / "dev/correctness-tools.json").read_text(encoding="utf-8"))
            receipt_path = output / "correctness-attestation.json"
            nextest_path = output / "cargo-nextest"
            need(receipt_path.stat().st_size < 4096 and nextest_path.stat().st_size <= 64 * 1024**2, "Correctness tool output exceeds bound")
            correctness = json.loads(receipt_path.read_text(encoding="utf-8"))
            nextest_hash = hashlib.sha256(nextest_path.read_bytes()).hexdigest()
            arch = {"x86_64": "x86_64", "aarch64": "aarch64"}.get(self.docker.info["Architecture"])
            need(arch in correctness_profile["nextest"]["architectures"] and correctness["architecture"] == arch and
                 correctness["revision"] == correctness_profile["nextest"]["revision"] and
                 correctness["archive_sha256"] == correctness_profile["nextest"]["architectures"][arch]["archive_sha256"] and
                 correctness["binary_sha256"] == nextest_hash and
                 correctness["version"].split()[:2] == ["cargo-nextest", correctness_profile["nextest"]["version"]] and
                 correctness["profile_sha256"] == hashlib.sha256(json.dumps(correctness_profile, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
                 "Correctness tool identity mismatch")
            (output / "navigation-tools.json").write_bytes((ROOT / "dev/navigation-tools.json").read_bytes())
            (output / ".dockerignore").write_text("*\n!lsp\n!cargo-nextest\n!correctness-attestation.json\n!navigation-tools.json\n", encoding="utf-8")
            final_source = (f"FROM {base_tag}\nCOPY --chmod=0755 lsp /usr/local/bin/lsp\n"
                            "COPY navigation-tools.json /opt/hsearch/navigation-tools.json\n"
                            "COPY --chmod=0755 cargo-nextest /usr/local/bin/cargo-nextest\n"
                            "COPY correctness-attestation.json /opt/hsearch/correctness-attestation.json\n"
                            f"RUN echo '{binary_hash.hexdigest()}  /usr/local/bin/lsp' | sha256sum --check - && lsp --version && "
                            f"echo '{nextest_hash}  /usr/local/bin/cargo-nextest' | sha256sum --check - && cargo-nextest --version\n"
                            "USER 10001:10001\n")
            tag = "hsearch-dev:" + digest[:20]
            final_iid = image_folder / (lease_id + ".final.iid")
            self.docker.command("build", "--file", "-", "--tag", tag, "--iidfile", str(final_iid), str(output),
                                input_text=final_source, stream=True, timeout=180)
            final_id = final_iid.read_text().strip()
            need(re.fullmatch(r"sha256:[a-f0-9]{64}", final_id) and image_digest() == digest, "Final image/source identity changed")
            info = json.loads(self.docker.command("image", "inspect", final_id))[0]
            need(info["Config"]["User"] == "10001:10001", "Development image must be non-root")
            atomic_json(image_folder / (digest + ".json"), {"id": info["Id"], "build_inputs_sha256": digest,
                        "dockerfile_sha256": hashlib.sha256(source).hexdigest(), "engine": self.docker.engine,
                        "navigation_artifact": artifact, "correctness_artifact": correctness})
            complete = True
            return info["Id"]
        finally:
            if complete:
                self.registry.release(lease_id)
            else:
                self.registry.update(lease_id, phase="uncertain-image-operation")

    def build_navigation(self, lease_id, image):
        # BuildKit has completed. Reuse this image workflow's existing BUILD
        # reservation for one compile container; do not stack a RustFS topology
        # and another BUILD reservation on small native runners.
        owner = secrets.token_hex(16)
        record = {"namespace": lease_id, "owner": owner, "image": image}
        with self.registry.edit() as data:
            data["leases"][lease_id].update(owner=owner, namespace=lease_id, image=image,
                                           phase="tool-preflight", create_pending=False, container_id=None)
        self.require_backend_reserve(self.backend_free(record, lease_id + "-admission"))
        folder = self.registry.root / "images" / (lease_id + "-build")
        need(folder.resolve() == folder.absolute() and not folder.exists(), "Build snapshot path redirected/reused")
        folder.mkdir(mode=0o700)
        freeze_harness(folder / "harness")
        output = folder / "artifacts"
        output.mkdir(mode=0o1777)
        output.chmod(0o1777)
        names = [lease_id + "-" + suffix for suffix in ("cargo", "target")]
        for name in names:
            self.docker.command("volume", "create", "--label", f"{LABEL}={owner}", "--label", f"io.hsearch.namespace={lease_id}", name)
        volumes = self.docker.inspect("volume", names)
        assert_owned(volumes, owner)
        need({item["Name"] for item in volumes} == set(names) and
             all(item["Labels"].get("io.hsearch.namespace") == lease_id for item in volumes), "Build volume identity changed")
        with self.registry.edit() as data:
            reserve = sum(item["budget"]["disk_bytes"] for item in data["leases"].values()) + 10 * GIB
        self.registry.update(lease_id, phase="tool-creating", create_pending=True, build_volumes=names,
                             artifact_directory=str(output))
        args = ["create", "--name", lease_id + "-compile", "--label", f"{LABEL}={owner}", "--label", f"io.hsearch.namespace={lease_id}",
                "--log-driver", "local", "--log-opt", "max-size=16m", "--log-opt", "max-file=1", "--log-opt", "compress=false",
                "--user", "10001:10001", "--read-only", "--cpus", "2", "--memory", str(4 * GIB), "--memory-swap", str(4 * GIB),
                "--pids-limit", "256", "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--network", "bridge",
                "--tmpfs", "/tmp:rw,nosuid,nodev,size=67108864", "--env", f"HSEARCH_BACKEND_RESERVE_BYTES={reserve}",
                "--mount", f"type=volume,source={names[0]},target=/cargo", "--mount", f"type=volume,source={names[1]},target=/target",
                "--mount", f"type=bind,source={folder / 'harness'},target=/harness,readonly",
                "--mount", f"type=bind,source={output},target=/navigation-output", image, "python3", "/harness/execute.py",
                "navigation-build", "--disk-bytes", str(8 * GIB), "--timeout", "1200", "--output-bytes", str(1024**2)]
        cid = self.docker.command(*args).strip()
        need(re.fullmatch(r"[a-f0-9]{64}", cid), "Unacknowledged tool-build create; retain image lease")
        self.registry.update(lease_id, container_id=cid, create_pending=False, phase="tool-compiling")
        self.run_container(record, cid)
        self.docker.command("start", cid)
        print(json.dumps({"navigation_build": "running", "container_id": cid, "reservation": BUILD}), flush=True)
        self.docker.command("logs", "--follow", cid, stream=True, timeout=1250)
        status = self.docker.command("wait", cid, timeout=10).strip()
        info = self.run_container(record, cid)
        need(status.isdigit() and not info["State"]["Running"] and info["State"]["ExitCode"] == int(status), "Tool-build outcome uncertain")
        self.docker.command("container", "rm", cid)
        self.registry.update(lease_id, container_id=None, phase="tool-exited", exit_code=int(status))
        volumes = self.docker.inspect("volume", names)
        assert_owned(volumes, owner)
        need({item["Name"] for item in volumes} == set(names) and
             all(item["Labels"].get("io.hsearch.namespace") == lease_id for item in volumes), "Build volume identity changed")
        self.docker.command("volume", "rm", *names)
        need(not self.docker.command("volume", "ls", "--filter", f"label=io.hsearch.namespace={lease_id}", "--format", "{{.Name}}").strip(),
             "Build volume cleanup uncertain")
        self.registry.update(lease_id, build_volumes=[], phase="tool-complete")
        need(int(status) == 0, "Navigation compilation failed; terminal image evidence retained")
        return output

    def up(self, task, *, task_claim=None, _bootstrap_image=None):
        need(task_claim is None or re.fullmatch(r"m[0-9]{2}-[0-9]{2}-[0-9a-f]{12}", task_claim), "Invalid task claim")
        digest = image_digest()
        image = _bootstrap_image or json.loads((self.registry.root / "images" / (digest + ".json")).read_text())
        if _bootstrap_image:
            builder = self.registry.get(image["bootstrap_owner"])
            need(builder["pid"] == os.getpid() and builder["kind"] == "image" and builder.get("bootstrap_image_id") == image["id"],
                 "Bootstrap image is not owned by this active builder")
        need(image["engine"] == self.docker.engine, "Development image belongs to another engine")
        need(image["build_inputs_sha256"] == image_digest() and
             image["dockerfile_sha256"] == hashlib.sha256((ROOT / "dev/Dockerfile").read_bytes()).hexdigest(), "Development image is stale")
        namespace = self.namespace(task)
        owner = secrets.token_hex(16)
        record = {"kind": "topology", "namespace": namespace, "owner": owner, "source": str(self.source_root),
                  "image": image["id"], "phase": "starting", "snapshot_schema": 2, "resource_protocol": 2, "task_claim": task_claim}
        self.registry.acquire(namespace, STORAGE, self.docker.capacity, self.docker.engine, record)
        folder = self.snapshot(record)
        prepared, backend_read_complete = False, False
        try:
            backend_free = self.backend_free(record, namespace + "-preflight")
            backend_read_complete = True
            self.require_backend_reserve(backend_free)
            folder.mkdir(parents=True, exist_ok=False)
            frozen = folder / "harness"
            hashes = freeze_harness(frozen)
            self.registry.update(namespace, harness_hashes=hashes)
            template = Template((ROOT / "dev/compose.json").read_text())
            # JSON-escape substituted path values before parsing the concrete snapshot.
            values = {"DEV_IMAGE": image["id"], "RUSTFS_IMAGE": self.images["rustfs"], "NAMESPACE": namespace,
                      "OWNER": owner, "SOURCE_ROOT": str(self.source_root).replace("\\", "/"),
                      "HARNESS_ROOT": str(frozen).replace("\\", "/")}
            config = json.loads(template.substitute({key: json.dumps(value)[1:-1] for key, value in values.items()}))
            path = folder / "compose.json"
            atomic_json(path, config)
            self.registry.update(namespace, config_sha256=hashlib.sha256(path.read_bytes()).hexdigest())
            prepared = True
            for suffix in VOLUMES:
                self.docker.command("volume", "create", "--label", f"{LABEL}={owner}", "--label", f"io.hsearch.namespace={namespace}", f"{namespace}-{suffix}")
                assert_owned(self.docker.inspect("volume", [f"{namespace}-{suffix}"]), owner)
            self.require_backend_reserve(self.backend_free(record, namespace + "-storage-admission", data_volumes=True))
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
        path = self.snapshot(record) / "compose.json"
        current = self.registry.get(record["namespace"])
        need(hashlib.sha256(path.read_bytes()).hexdigest() == current["config_sha256"], "Owned Compose snapshot changed")
        return self.docker.command("compose", "--project-name", record["namespace"], "--file", str(path), *args, timeout=150)

    def probe_storage(self, namespace):
        """Inspect the actual owned pinned server without exposing Config/Env."""
        record = self.registry.get(namespace)
        need(record.get("source") == str(self.source_root) and record["engine"] == self.docker.engine and
             record.get("task_claim") and record.get("resource_protocol") == 2, "Probe storage owner/source unavailable")
        info = self.docker.inspect("container", record["containers"])
        assert_owned(info, record["owner"])
        servers = [item for item in info if item["Config"]["Labels"].get("com.docker.compose.service") == "rustfs"]
        need(len(servers) == 1 and servers[0]["Id"] in record["containers"], "Probe storage CID changed")
        server = servers[0]
        state = server.get("State", {})
        need(state.get("Running") is True and state.get("Status") == "running" and type(state.get("Pid")) is int
             and state["Pid"] > 0, "Probe server is not a known running process")
        need(server["Config"]["Labels"].get("io.hsearch.namespace") == namespace, "Probe storage namespace changed")
        pinned = json.loads(self.docker.command("image", "inspect", self.images["rustfs"]))
        need(len(pinned) == 1 and pinned[0].get("Id") == server.get("Image") and
             any(value.rsplit("@", 1)[-1] == self.images["rustfs"].rsplit("@", 1)[-1]
                 for value in pinned[0].get("RepoDigests", [])), "Probe actual server does not match pinned image")
        data = [m for m in server.get("Mounts", []) if m.get("Destination") == "/data"]
        need(len(data) == 1 and data[0].get("Name") == namespace+"-data" and data[0].get("Type") == "volume"
             and data[0].get("RW") is True, "Probe storage data volume changed")
        networks = server.get("NetworkSettings", {}).get("Networks", {})
        need(namespace+"-net" in networks and networks[namespace+"-net"].get("NetworkID") == record["network"]
             and not any(server["NetworkSettings"].get("Ports", {}).values())
             and not server.get("HostConfig", {}).get("PortBindings"), "Probe private server network changed")
        expected = {"image": server["Image"], "cid": server["Id"], "owner": record["owner"], "namespace": namespace,
                    "volume": data[0]["Name"], "config_sha256": hashlib.sha256(json.dumps(server["Config"], sort_keys=True,
                    separators=(",", ":"), ensure_ascii=True).encode()).hexdigest()}
        return server, expected

    def restart_probe_storage(self, namespace, persist_intent, persist_observation):
        """One normal restart of the exact known server, after known child release.

        persist_intent is the normal charged evidence callback. Unknown outcomes
        retain a busy topology/intent; no automatic repair or forced cleanup.
        """
        before, expected = self.probe_storage(namespace)
        token = secrets.token_hex(16)
        persist_intent({"operation": "same-volume-restart", "token": token, "cid": expected["cid"],
                        "image": expected["image"], "config_sha256": expected["config_sha256"],
                        "volume_sha256": hashlib.sha256(expected["volume"].encode()).hexdigest()})
        with self.registry.edit() as data:
            record = data["leases"][namespace]
            need(record["phase"] == "ready" and not record.get("probe_restart_pending") and
                 not any(value.get("parent") == namespace for value in data["leases"].values()),
                 "Probe restart requires exclusive topology and released child")
            record.update(phase="probe-restarting", probe_restart_pending=True, probe_restart_token=token)
        version = self.docker.command("version", "--format", "{{.Client.Version}}", output_bytes=1024).strip()
        need(0 < len(version.encode()) <= 128, "Probe restart CLI version unavailable")
        response = self.docker.command("restart", "--timeout", "10", expected["cid"],
                                       timeout=60, output_bytes=4096, raw_streams=True, capture_status=True)
        # Persist exact bounded returned bytes before parsing them. Neither a
        # warning nor a successful CLI exit can replace the exact CID proof.
        persist_observation({"client_version": version, "expected_cid": expected["cid"],
                             "streams": response["streams"], "exit_code": response["exit_code"],
                             "command": "restart --timeout 10"})
        need(response["exit_code"] == 0 and response["streams"]["stdout"].strip() == expected["cid"].encode("ascii"),
             "Probe restart outcome unknown; intent and charges retained")
        after, _actual = self.probe_storage(namespace)
        controller = runpy.run_path(str(ROOT / "tools/rustfs_probe.py"))
        controller["restart_receipt"](before, after, expected)
        with self.registry.edit() as data:
            current = data["leases"][namespace]
            need(current.get("probe_restart_token") == token and current.get("probe_restart_pending") is True,
                 "Probe restart owner changed")
            current.update(phase="ready", probe_restart_pending=False)
        return before, after, expected

    def prepare_probe_inputs(self, namespace, mode, files, binding, build=None):
        """Prepare one authenticated fixed-profile input slot, never arbitrary mounts.

        Called only by the normal source/packet/evidence callback integration.
        All public bytes must first be admitted by its charged evidence callback.
        """
        need(mode in PROBE_MODES and type(files) is dict and type(binding) is dict, "Invalid fixed probe input")
        record = self.registry.get(namespace)
        need(record.get("source") == str(self.source_root) and record.get("phase") == "ready"
             and record.get("task_claim") == binding.get("claim") and binding.get("owner")
             and record.get("resource_protocol") == 2, "Probe task/source ownership changed")
        required = ({"config.json"} if mode == "probe-checker-build" else
                    {"config.json", "plan.wire"} if mode.endswith("before") else
                    {"config.json", "plan.wire", "prior.history"} if mode.endswith("after") else
                    {"plan.wire", "capture.history", "restart.json", "admission.sha256"})
        need(set(files) == required and sum(len(raw) for raw in files.values()) <= 1024**2, "Probe input roster/aggregate changed")
        if mode != "probe-checker-build":
            need(type(build) is dict and build.get("source_sha256") == binding.get("source_sha256") and
                 re.fullmatch(r"[a-f0-9]{64}", build.get("sha256", "")), "Checker attestation unavailable")
        root = self.snapshot(record) / "rustfs-probe-inputs"
        need(root.resolve() == root.absolute() and not root.is_symlink(), "Probe input path redirected")
        root.mkdir(exist_ok=True, mode=0o755)
        directory = root / mode
        need(not directory.exists(), "Probe phase slot already used; retain first outcome")
        directory.mkdir(mode=0o755)
        pins = {}
        for name, raw in files.items():
            cap = 704*1024 if name.endswith(".history") else 65536 if name in {"config.json", "plan.wire"} else 1024
            need(type(raw) is bytes and 0 < len(raw) <= cap, "Probe input exceeds fixed file bound")
            path = directory / name
            with path.open("xb") as output:
                output.write(raw)
                output.flush()
                os.fsync(output.fileno())
            path.chmod(0o444)
            pins[name] = {"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}
        current = dict(record.get("rustfs_probe_inputs", {}))
        need(mode not in current, "Probe input receipt already published")
        current[mode] = {"pins": pins, "binding": binding, "build": build}
        self.registry.update(namespace, rustfs_probe_inputs=current)
        return {"bytes": sum(pin["bytes"] for pin in pins.values()), "pins": pins}

    def probe_input_directory(self, record, mode):
        receipt = record.get("rustfs_probe_inputs", {}).get(mode)
        need(type(receipt) is dict and receipt["binding"]["claim"] == record.get("task_claim"), "Probe inputs not source-admitted")
        directory = self.snapshot(record) / "rustfs-probe-inputs" / mode
        need(directory.resolve() == directory.absolute() and not directory.is_symlink() and
             set(p.name for p in directory.iterdir()) == set(receipt["pins"]), "Probe input roster redirected/changed")
        for name, pin in receipt["pins"].items():
            path = directory / name
            need(not path.is_symlink() and path.is_file() and path.stat().st_nlink == 1 and
                 path.stat().st_size == pin["bytes"] <= 704*1024, "Probe input type/extent changed")
            need(hashlib.sha256(path.read_bytes()).hexdigest() == pin["sha256"], "Probe input bytes changed")
        return directory

    def probe_execution(self, record, container_id, mode, budget):
        info = self.run_container(record, container_id)
        need(info.get("Image") == record["image"], "Probe actual image changed or missing")
        config, host = info.get("Config", {}), info.get("HostConfig", {})
        network = record["namespace"]+"-net" if mode in PROBE_CLIENT else "none"
        expected = {"NanoCpus": budget["cpu_millis"]*1000000, "Memory": budget["memory_bytes"],
                    "MemorySwap": budget["memory_bytes"], "PidsLimit": budget["pids"], "ReadonlyRootfs": True,
                    "NetworkMode": network}
        need(config.get("User") == "10001:10001" and all(host.get(k) == v for k, v in expected.items()), "Probe actual isolation/limits changed")
        expected_tmp = "rw,nosuid,nodev,size="+str(1024**2 if mode in PROBE_CLIENT else 67108864)
        need(host.get("Tmpfs", {}) == {"/tmp": expected_tmp}, "Probe fixed writable tmpfs changed")
        writable = {m.get("Destination") for m in info.get("Mounts", []) if m.get("RW")}
        need(writable <= ({"/cargo", "/target", "/tmp"} if mode == "probe-checker-build" else {"/tmp"}), "Probe has undeclared writable mount")
        mounts = info.get("Mounts", [])
        destinations = [m.get("Destination") for m in mounts]
        expected_ro = {"/workspace", "/harness", "/probe-input"}
        expected_ro |= set() if mode == "probe-checker-build" else {"/probe-checker"}
        if mode in PROBE_CLIENT:
            expected_ro |= {"/certs", "/run/secrets"}
        allowed = expected_ro | ({"/cargo", "/target", "/tmp"} if mode == "probe-checker-build" else {"/tmp"})
        need(len(destinations) == len(set(destinations)) and set(destinations) <= allowed and
             expected_ro <= set(destinations) and all(not m.get("RW") for m in mounts if m.get("Destination") in expected_ro),
             "Probe fixed mount roster/read-only proof changed")
        sources = {"/workspace": self.source_root, "/harness": self.harness(record),
                   "/probe-input": self.probe_input_directory(record, mode)}
        for destination, source in sources.items():
            selected = next(m for m in mounts if m["Destination"] == destination)
            need(selected.get("Type") == "bind" and selected.get("Source") == str(source),
                 "Probe actual source/harness/input bind changed")
        for suffix, destination in (("target", "/probe-checker"), ("client", "/certs"), ("credentials", "/run/secrets")):
            if destination in expected_ro:
                selected = next(m for m in mounts if m["Destination"] == destination)
                need(selected.get("Type") == "volume" and selected.get("Name") == record["namespace"]+"-"+suffix,
                     "Probe exact read-only volume changed")
        images = json.loads(self.docker.command("image", "inspect", record["image"]))
        need(len(images) == 1 and images[0].get("Id") == record["image"] and
             images[0].get("Architecture") in {"amd64", "arm64"}, "Probe image/platform identity changed")
        state = info["State"]
        return {"container_id": container_id, "image_id": info["Image"], "engine": self.docker.engine,
                "architecture": "x86_64" if images[0]["Architecture"] == "amd64" else "aarch64",
                "user": config["User"], "read_only": True, "network": network, "budget": budget,
                "state": {"status": state["Status"], "running": state["Running"], "exit_code": state["ExitCode"]}}

    def run(self, namespace, mode, *, output_budget=1024**2, stream=True, artifact_directory=None, checkpoint=None, checkpoint_context=None, deadline=None):
        need(mode in {"prefetch", "diagnostics", "check", "compile", "probe", "navigation-build", "coverage", "durability-sample", "durability-object"} | PROBE_MODES, "Unknown sandbox command")
        need(type(output_budget) is int and 4096 <= output_budget <= 8 * 1024**2, "Invalid command output budget")
        record = self.registry.get(namespace)
        need(record.get("source") == str(self.source_root) and record["engine"] == self.docker.engine and record["phase"] == "ready", "Namespace unavailable in this worktree/engine")
        native = None
        objects = None
        if mode == "durability-object":
            objects = runpy.run_path(str(self.source_root / "tools/object_durability_campaign.py"))
            need(objects["BUDGET"] == OBJECT, "Object broker envelope differs")
            need(output_budget >= 512 * 1024, "Object broker requires its fixed512KiB diagnostic profile")
            output_budget = min(output_budget, 512 * 1024)
            need(artifact_directory == self.snapshot(record) / "object-evidence" and
                 artifact_directory.resolve() == artifact_directory.absolute() and not artifact_directory.exists(),
                 "Object output escaped or reused its owned snapshot")
            object_input_path = self.snapshot(record) / "object-binding.json"
            object_raw = objects["read"](object_input_path, 65536)
            need(record.get("object_durability_input") == {"bytes": len(object_raw), "sha256": hashlib.sha256(object_raw).hexdigest()},
                 "Object input was not prepared by the owner")
            object_inputs = objects["decode"](object_raw)
            object_config = objects["worker_config"](record, self.source_root, self.harness(record), object_input_path, artifact_directory)
            object_config["target"] = self.docker.info["Architecture"] + "-unknown-linux-gnu"
            objects["validate_inputs"](object_inputs, record, object_config, self.images["rustfs"], self.source_root)
            objects["source_bookend"](self.source_root, object_inputs["binding"])
        if mode == "durability-sample":
            native = runpy.run_path(str(self.source_root / "tools/native_durability_campaign.py"))
            need(artifact_directory == self.snapshot(record) / "durability-evidence" and
                 artifact_directory.resolve() == artifact_directory.absolute() and not artifact_directory.exists(),
                 "Native output escaped or reused its owned snapshot")
            input_path = self.snapshot(record) / "durability-binding.json"
            input_raw = native["read"](input_path, 4096)
            need(record.get("native_durability_input") == {"bytes": len(input_raw), "sha256": hashlib.sha256(input_raw).hexdigest()},
                 "Native input was not prepared by the owner")
            native_binding = native["decode"](input_raw)
            native["validate_input"](native_binding)
            need(native_binding["image"] == record["image"] and
                 native_binding["target"] == self.docker.info["Architecture"] + "-unknown-linux-gnu", "Native image/target binding")
        need(mode not in {"durability-sample", "durability-object"} or checkpoint is not None,
             "Durability workers require trusted pre-disposal checkpoint")
        if checkpoint is not None:
            need(callable(checkpoint) and type(checkpoint_context) is dict and deadline is not None,
                 "Trusted campaign checkpoint context required")
            # Full worker wait, bounded Docker finalization and content barrier.
            deadline.require(1250 + 12 * 180 + 600)
        if mode == "navigation-build":
            need(artifact_directory == self.snapshot(record) / "artifacts" and artifact_directory.resolve() == artifact_directory.absolute()
                 and not artifact_directory.exists(), "Navigation artifact output escaped or reused the owned snapshot")
        elif mode == "coverage":
            need(record.get("snapshot_schema") == 2 and self.source_root == ROOT and
                 artifact_directory == self.snapshot(record) / "coverage-evidence" and
                 artifact_directory.resolve() == artifact_directory.absolute() and not artifact_directory.exists(),
                 "Coverage output escaped or reused its owned snapshot")
            pins = record.get("coverage_inputs")
            need(type(pins) is dict and set(pins) == {"coverage-source.bundle", "coverage-source.json"}, "Coverage inputs were not prepared by the owner")
            for name, pin in pins.items():
                path = self.snapshot(record) / name
                need(path.resolve() == path.absolute() and path.is_file() and path.stat().st_nlink == 1 and
                     type(pin) is dict and set(pin) == {"bytes", "sha256"} and type(pin["bytes"]) is int and
                     0 < pin["bytes"] <= (128 * 1024**2 if name.endswith(".bundle") else 4096) and path.stat().st_size == pin["bytes"],
                     "Invalid owned coverage input snapshot")
                before = path.stat()
                digest, count = hashlib.sha256(), 0
                with path.open("rb") as source:
                    while chunk := source.read(65536):
                        count += len(chunk)
                        need(count <= pin["bytes"], "Coverage input changed beyond its bound")
                        digest.update(chunk)
                after = path.stat()
                need(count == pin["bytes"] and digest.hexdigest() == pin["sha256"] and
                     (before.st_ino, before.st_size, before.st_mtime_ns) == (after.st_ino, after.st_size, after.st_mtime_ns),
                     "Coverage input snapshot changed")
        elif mode not in {"durability-sample", "durability-object"}:
            need(artifact_directory is None, "Unexpected artifact output mount")
        harness = self.harness(record)
        probe_directory = None
        if mode in PROBE_MODES:
            need(record.get("snapshot_schema") == 2 and record.get("resource_protocol") == 2 and
                 all((harness/name).is_file() for name in PROBE_HARNESS) and
                 output_budget >= 768*1024+4096, "Fixed probe source/framing unavailable")
            probe_directory = self.probe_input_directory(record, mode)
        if record.get("resource_protocol") == 2:
            expected = {f"{namespace}-{suffix}" for suffix in VOLUMES}
            volumes = self.docker.inspect("volume", sorted(expected))
            need(len(volumes) == len(expected) and {item["Name"] for item in volumes} == expected, "Task volume identity changed")
            assert_owned(volumes, record["owner"])
            need(all(item["Labels"].get("io.hsearch.namespace") == namespace for item in volumes), "Task volume namespace changed")
        lease_id = self.namespace("run")
        budget = OBJECT if objects is not None else PROBE if mode == "probe" or mode in PROBE_CLIENT else BUILD
        with self.registry.edit() as data:
            need(data["leases"][namespace]["phase"] == "ready", "Namespace lifecycle is busy")
            need(not any(x.get("parent") == namespace for x in data["leases"].values()), "Mutable cache/target is already leased")
            admit(budget, data["leases"].values(), self.docker.capacity, self.registry.free_disk())
            data["leases"][lease_id] = {"kind": "run", "parent": namespace, "engine": self.docker.engine, "budget": budget,
                                         "pid": os.getpid(), "process_identity": owner_identity()}
        container = namespace + "-" + lease_id[-12:]
        args = ["create", "--name", container, "--label", f"{LABEL}={record['owner']}", "--label", f"io.hsearch.namespace={namespace}",
                "--log-driver", "local", "--log-opt", "max-size=16m", "--log-opt", "max-file=1", "--log-opt", "compress=false",
                "--user", "10001:10001", "--cpus", str(budget["cpu_millis"] / 1000), "--memory", str(budget["memory_bytes"]), "--pids-limit", str(budget["pids"]), "--read-only",
                "--memory-swap", str(budget["memory_bytes"]),
                "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--tmpfs", "/tmp:rw,nosuid,nodev,size=67108864",
                "--network", "bridge" if mode in {"prefetch", "navigation-build"} else (namespace + "-net" if mode in {"probe", "durability-object"} else "none"),
                "--mount", f"type=bind,source={self.source_root},target=/workspace,readonly",
                "--mount", f"type=bind,source={harness},target=/harness,readonly", "--mount", f"type=volume,source={namespace}-cargo,target=/cargo",
                "--mount", f"type=volume,source={namespace}-target,target=/target"]
        if objects is not None:
            # The object driver gets no compiler caches or writable source.
            args = args[:args.index("--mount")]
            args += ["--mount", f"type=bind,source={self.source_root},target=/workspace,readonly",
                     "--mount", f"type=bind,source={harness},target=/harness,readonly",
                     "--mount", f"type=bind,source={object_input_path},target=/object-input/binding.json,readonly",
                     "--mount", f"type=bind,source={artifact_directory},target=/object-output",
                     "--mount", f"type=volume,source={namespace}-client,target=/certs,readonly,volume-nocopy",
                     "--mount", f"type=volume,source={namespace}-credentials,target=/run/secrets,readonly,volume-nocopy",
                     "--mount", f"type=volume,source={namespace}-data,target=/object-store-accounting,readonly,volume-nocopy"]
        if native is not None:
            args += ["--mount", f"type=bind,source={artifact_directory},target=/durability-evidence",
                     "--mount", f"type=bind,source={input_path},target=/durability-binding.json,readonly"]
        if mode in PROBE_MODES:
            # Rebuild the fixed profile's mounts, rather than hiding prefilled
            # writable build caches from its disk monitor.
            first_mount = args.index("--mount")
            args = args[:first_mount]
            tmpfs = args.index("--tmpfs") + 1
            args[tmpfs] = "/tmp:rw,nosuid,nodev,size=" + str(1024**2 if mode in PROBE_CLIENT else 67108864)
            network_index = args.index("--network") + 1
            args[network_index] = namespace+"-net" if mode in PROBE_CLIENT else "none"
            args += ["--env", "PYTHONDONTWRITEBYTECODE=1",
                     "--mount", f"type=bind,source={self.source_root},target=/workspace,readonly",
                     "--mount", f"type=bind,source={harness},target=/harness,readonly",
                     "--mount", f"type=bind,source={probe_directory},target=/probe-input,readonly"]
            if mode == "probe-checker-build":
                args += ["--mount", f"type=volume,source={namespace}-cargo,target=/cargo",
                         "--mount", f"type=volume,source={namespace}-target,target=/target"]
            else:
                args += ["--mount", f"type=volume,source={namespace}-target,target=/probe-checker,readonly"]
                receipt = record["rustfs_probe_inputs"][mode]
                args += ["--env", "HSEARCH_CHECKER_SHA256=" + receipt["build"]["sha256"]]
            if mode in PROBE_CLIENT:
                args += ["--mount", f"type=volume,source={namespace}-client,target=/certs,readonly",
                         "--mount", f"type=volume,source={namespace}-credentials,target=/run/secrets,readonly"]
        if mode == "navigation-build":
            args += ["--mount", f"type=bind,source={artifact_directory},target=/navigation-output"]
        if mode == "coverage":
            args += ["--mount", f"type=bind,source={artifact_directory},target=/coverage-evidence"]
            for name in ("coverage-source.bundle", "coverage-source.json"):
                args += ["--mount", f"type=bind,source={self.snapshot(record) / name},target=/{name},readonly"]
        if mode == "probe":
            args += ["--mount", f"type=volume,source={namespace}-client,target=/certs,readonly",
                     "--mount", f"type=volume,source={namespace}-credentials,target=/run/secrets,readonly", record["image"], "python3", "/harness/probe.py", namespace]
        else:
            args += [record["image"], "python3", "/harness/execute.py", mode, "--disk-bytes", str(budget["disk_bytes"]),
                     "--timeout", "120" if mode in PROBE_CLIENT else "1200", "--output-bytes", str(output_budget)]
            if mode in PROBE_CLIENT:
                index = args.index("--disk-bytes")+1
                args[index] = str(PROBE["disk_bytes"])
        backend_known, workload_attempted = False, False
        self.registry.update(lease_id, protocol=2, phase="reserved", container_name=container, mode=mode, owner_active=True, checkpoint_required=checkpoint is not None)
        try:
            free = self.backend_free(record, container + "-admission", data_volumes=True)
            backend_known = True
            self.require_backend_reserve(free)
            if artifact_directory is not None:
                artifact_directory.mkdir(mode=0o1777)
                artifact_directory.chmod(0o1777)
            workload_attempted = True
            self.registry.update(lease_id, protocol=2, phase="creating", container_name=container, mode=mode)
            container_id = self.docker.command(*args).strip()
            need(re.fullmatch(r"[0-9a-f]{64}", container_id), "Uncertain container-create identity; lease retained")
            self.registry.update(lease_id, phase="created", container_id=container_id)
            created_info = self.run_container(record, container_id)
            if native is not None:
                native["verify_container"](created_info, record, self.source_root, harness, input_path, artifact_directory)
            if objects is not None:
                object_topology = objects["verify_topology"](self, record)
                objects["verify_container"](created_info, record, object_config)
            if mode in PROBE_MODES:
                self.probe_execution(record, container_id, mode, budget)
            self.registry.update(lease_id, phase="starting")
            self.docker.command("start", container_id)
            if objects is not None:
                objects["verify_container"](self.run_container(record, container_id), record, object_config, require_endpoint=True)
            self.registry.update(lease_id, phase="running")
            status = self.docker.command("wait", container_id, timeout=1250).strip()
            need(re.fullmatch(r"[0-9]{1,3}", status), "Unknown workload outcome; lease retained")
            info = self.run_container(record, container_id)
            execution = self.probe_execution(record, container_id, mode, budget) if mode in PROBE_MODES else None
            need(info["State"]["Status"] in {"exited", "dead"} and not info["State"]["Running"] and
                 info["State"]["ExitCode"] == int(status), "Workload completion changed; lease retained")
            if mode == "check":
                execution = self.public_execution(record, info, budget, output_budget)
            streams = None
            if mode in PROBE_MODES or checkpoint is not None or native is not None or objects is not None:
                streams = self.docker.command("logs", container_id, output_bytes=output_budget, raw_streams=True)
                raw = streams["stdout"]
                try:
                    logs = raw.decode("utf-8", errors="strict")
                    encoding = "utf-8-strict"
                except UnicodeError:
                    logs, encoding = raw.decode("utf-8", errors="replace"), "invalid-utf8-retained"
            else:
                logs = self.docker.command("logs", container_id, merge_output=True)
            raw = logs.encode() if streams is None else streams["stdout"]
            result = {"mode": mode, "container_id": container_id, "exit_code": int(status), "encoding": "utf-8-replacement",
                      "output": raw[:output_budget].decode(errors="ignore"), "truncated": len(raw) > output_budget}
            if streams is not None:
                result.update(output=logs, encoding=encoding, raw_streams=streams,
                    output_protocol="rustfs-raw-streams-v1", stderr_bytes=len(streams["stderr"]),
                    child_stream_semantics="stdout-and-stderr-merged-by-monitor")
            if execution is not None:
                result["execution"] = execution
            if native is not None:
                self.registry.update(lease_id, native_export_verified=False)
            if objects is not None:
                self.registry.update(lease_id, object_export_verified=False)
            def export_files():
                if artifact_directory is None:
                    return {}
                rows = {}
                total = 0
                for path in artifact_directory.iterdir():
                    need(len(rows) < 256 and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", path.name), "Campaign export inventory bound")
                    before = path.lstat()
                    need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1 and before.st_size <= 1024**2,
                         "Campaign export file type/link/size")
                    with path.open("rb") as handle:
                        raw = handle.read(1024**2 + 1)
                    after = path.lstat()
                    need(len(raw) == before.st_size and before == after, "Campaign export changed during binding")
                    total += len(raw)
                    need(total <= 32*1024**2, "Campaign export aggregate bound")
                    rows[path.name] = {"bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()}
                return rows
            admitted_export = export_files()
            checkpoint_attempted = False
            try:
                if native is not None:
                    need(int(status) == 0 and not result["truncated"], "Native worker failed or truncated")
                    need(native["read"](input_path, 4096) == input_raw, "Native input changed")
                    native["verify_container"](info, record, self.source_root, harness, input_path, artifact_directory)
                    result["native_durability"] = native["verify_export"](artifact_directory, native_binding)
                if objects is not None:
                    need(int(status) == 0 and not result["truncated"], "Object worker failed or truncated")
                    need(objects["read"](object_input_path, 65536) == object_raw and
                         objects["verify_topology"](self, record) == object_topology, "Object input/topology changed")
                    objects["verify_container"](info, record, object_config)
                    proof = objects["verify_export"](artifact_directory, object_inputs, streams["stdout"], container_id, object_config)
                    proof["authenticated_topology"] = {**object_topology, "worker_cid": container_id}
                    result["object_durability"] = proof
                need(export_files() == admitted_export, "Campaign export changed across independent oracle")
                result["export_files"] = admitted_export
                if checkpoint is not None:
                    terminal = self.run_container(record, container_id)["State"]
                    need(not terminal["Running"] and terminal["Status"] in {"exited", "dead"} and
                         terminal["ExitCode"] == int(status), "Checkpoint terminal state changed")
                    context = {**checkpoint_context, "container_id": container_id, "namespace": namespace,
                               "run_lease": lease_id, "mode": mode, "image": record["image"],
                               "budget": budget, "harness_hashes": record.get("harness_hashes", {}),
                               "image_inputs_sha256": image_digest(), "config_sha256": record.get("config_sha256"),
                               "terminal": {"running": False, "exit_code": int(status)}}
                    checkpoint_attempted = True
                    reference = checkpoint(context, result, artifact_directory)
                    self.registry.update(lease_id, campaign_checkpoint=reference)
                    barrier = runpy.run_path(str(ROOT / "tools/campaign_checkpoint.py"))
                    barrier["sync_file"](self.registry.path)
                    barrier["sync_directory"](self.registry.root)
                    result["checkpoint"] = reference
            except Exception as error:
                if checkpoint is not None and not checkpoint_attempted:
                    result["verification_failed"] = True
                    try:
                        result["export_files"] = export_files()
                        terminal_context = {**checkpoint_context, "container_id": container_id, "namespace": namespace,
                            "run_lease": lease_id, "mode": mode, "image": record["image"], "budget": budget,
                            "terminal": {"running": False, "exit_code": int(status)}}
                        checkpoint(terminal_context, result, artifact_directory)
                    except Exception as retention_error:
                        error.add_note("Failure checkpoint unavailable; worker and lease retained: " + type(retention_error).__name__)
                raise
            self.docker.command("container", "rm", container_id)
            need(not self.docker.command("container", "ls", "--all", "--no-trunc", "--filter", f"id={container_id}", "--format", "{{.ID}}").strip(),
                 "Completed container removal uncertain; lease retained")
            self.registry.release(lease_id)
            if stream:
                print(result["output"], end="", flush=True)
            if result["exit_code"] or result["truncated"]:
                raise WorkloadFailed(result)
            return result
        finally:
            # An empty listing is not proof that an uncertain create cannot arrive later.
            # Known-ID removal above, or a never-attempted workload, permits release.
            if backend_known and not workload_attempted:
                self.registry.release(lease_id)
            with self.registry.edit() as data:
                if lease_id in data["leases"]:
                    data["leases"][lease_id]["owner_active"] = False

    def run_container(self, record, container_id):
        info = self.docker.inspect("container", [container_id])
        need(len(info) == 1 and info[0]["Id"] == container_id, "Run container identity changed")
        assert_owned(info, record["owner"])
        need(info[0].get("Image") == record["image"], "Run container actual image changed or missing")
        need(info[0]["Config"]["Labels"].get("io.hsearch.namespace") == record["namespace"], "Run container namespace changed")
        return info[0]

    def public_execution(self, record, info, budget, output_budget):
        """Allowlist actual pre-removal inspect fields; never export Env or mounts."""
        image = json.loads(self.docker.command("image", "inspect", record["image"]))[0]
        need(image["Id"] == info["Image"] == record["image"], "Admitted actual image changed")
        architecture = {"amd64": "x86_64", "arm64": "aarch64"}.get(image["Architecture"])
        need(architecture is not None and architecture == self.docker.info["Architecture"], "Actual native image architecture changed")
        config, host, state = info["Config"], info["HostConfig"], info["State"]
        return {"container_id": info["Id"], "image_id": info["Image"], "architecture": architecture,
                "engine": self.docker.engine, "user": config["User"], "read_only": host["ReadonlyRootfs"],
                "network": host["NetworkMode"], "limits": {"cpu_nanos": host["NanoCpus"], "memory": host["Memory"],
                "memory_swap": host["MemorySwap"], "pids": host["PidsLimit"]},
                "state": {"status": state["Status"], "running": state["Running"], "exit_code": state["ExitCode"]},
                "budget": dict(budget), "output_budget": output_budget}

    def reconcile(self, namespace, *, checkpoint_verify=None):
        """Reconcile only acknowledged container IDs after their worker exited."""
        record = self.registry.get(namespace)
        need(record["source"] == str(self.source_root) and record["engine"] == self.docker.engine, "Foreign recovery namespace")
        recovered = []
        with self.registry.edit() as data:
            candidates = [(key, dict(item)) for key, item in data["leases"].items() if item.get("parent") == namespace]
        for lease_id, candidate in candidates:
            container_id = candidate.get("container_id", "")
            need(candidate.get("protocol") == 2 and isinstance(container_id, str) and re.fullmatch(r"[0-9a-f]{64}", container_id),
                 "Uncertain unacknowledged create cannot be reclaimed")
            if candidate.get("checkpoint_required"):
                need(callable(checkpoint_verify) and candidate.get("campaign_checkpoint"),
                     "Campaign checkpoint absent; retain acknowledged worker and lease")
                checkpoint_verify(candidate["campaign_checkpoint"])
            token = secrets.token_hex(12)
            with self.registry.edit() as data:
                current = data["leases"][lease_id]
                identity = owner_identity(current["pid"])
                need(current.get("owner_active") is False or identity is None or identity != "unknown" and identity != current["process_identity"],
                     "Run worker/reconciler is live or uncertain")
                need(current.get("container_id") == container_id, "Run identity changed during reconciliation")
                current.update(phase="reconciling", pid=os.getpid(), process_identity=owner_identity(), reconcile_token=token, owner_active=True)
            visible = self.docker.command("container", "ls", "--all", "--no-trunc", "--filter", f"id={container_id}", "--format", "{{.ID}}").split()
            if visible:
                need(visible == [container_id], "Unexpected recovery container")
                info = self.run_container(record, container_id)
                if info["State"]["Running"] or info["State"]["Status"] in {"paused", "restarting"}:
                    with self.registry.edit() as data:
                        need(data["leases"][lease_id]["reconcile_token"] == token, "Recovery owner changed")
                        data["leases"][lease_id] = candidate
                    raise Refused("Known workload is still running; no duplicate or forced cleanup")
                # rm without force refuses a concurrent start. Once removal is
                # acknowledged, a queued start cannot resurrect this known ID.
                self.docker.command("container", "rm", container_id)
            need(not self.docker.command("container", "ls", "--all", "--no-trunc", "--filter", f"id={container_id}", "--format", "{{.ID}}").strip(),
                 "Recovery container removal uncertain")
            self.registry.release(lease_id)
            recovered.append({"lease": lease_id, "container_id": container_id, "outcome": "interrupted; rerun checks required"})
        return recovered

    def down(self, namespace):
        record = self.registry.get(namespace)
        need(record.get("source") == str(self.source_root) and record["engine"] == self.docker.engine, "Cleanup belongs to another worktree/engine")
        with self.registry.edit() as data:
            need(not any(x.get("parent") == namespace for x in data["leases"].values()), "Cleanup refused: active or uncertain run lease")
            current = data["leases"][namespace]
            need(not current.get("probe_restart_pending"), "Cleanup refused: unknown probe restart retained")
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

    def smoke(self, evidence_parent=None):
        differential = runpy.run_path(str(ROOT / "tools/engine_differential.py")) if evidence_parent is not None else None
        area = differential["evidence_directory"](evidence_parent, self.source_root, self.registry.root) if differential else None
        before = differential["source_identity"](self.source_root) if differential else None
        namespaces = []
        try:
            first = self.up("smoke-a")
            namespaces.append(first)
            for mode in ("prefetch", "diagnostics", "check", "probe"):
                try:
                    result = self.run(first, mode)
                except WorkloadFailed as error:
                    if area is not None and mode == "check":
                        differential["save_public"](area / "failed-check.json", error.result, 2 * 1024**2)
                    raise
                if area is not None and mode == "check":
                    differential["save_public"](area / "check-result.json", result, 2 * 1024**2)
                    record = self.registry.get(first)
                    cache = json.loads((self.registry.root / "images" / (image_digest() + ".json")).read_text(encoding="utf-8"))
                    need(cache["id"] == record["image"], "Smoke admitted image changed")
                    image = {key: cache[key] for key in ("id", "engine", "build_inputs_sha256", "dockerfile_sha256", "correctness_artifact")}
                    image["correctness_artifact"] = {key: cache["correctness_artifact"][key] for key in ("architecture", "revision", "archive_sha256", "profile_sha256", "binary_sha256", "version")}
                    image["architecture"] = result["execution"]["architecture"]
                    capture = differential["collect_native"](result, before, differential["source_identity"](self.source_root), image, self.source_root)
                    differential["save_public"](area / "capture.json", capture)
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
    parser.add_argument("--evidence-parent", type=Path)
    parser.add_argument("--state-root", type=Path, default=Path(tempfile.gettempdir()) / "hsearch-dev-leases")
    parser.add_argument("command", choices=("image", "up", "run", "down", "status", "smoke"))
    parser.add_argument("target", nargs="?")
    parser.add_argument("mode", nargs="?", choices=("prefetch", "check", "compile", "probe", "diagnostics"))
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
            sandbox.smoke(args.evidence_parent)
        else:
            with sandbox.registry.edit() as data:
                print(json.dumps(data, indent=2))
        return 0
    except (Refused, OSError, ValueError, KeyError) as error:
        print(f"REFUSED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
