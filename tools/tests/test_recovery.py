"""Counterexamples for acknowledged-ID recovery and frozen monitor ownership."""
import hashlib
import json
import os
from pathlib import Path
import runpy
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

API = runpy.run_path(str(Path(__file__).resolve().parents[1] / "sandbox.py"))
Sandbox, Refused = API["Sandbox"], API["Refused"]
GIB = API["GIB"]
NAMESPACE = "hsearch-test-0123456789ab"
CID = "a" * 64
CAPACITY = {"cpu_millis": 8000, "memory_bytes": 16 * GIB, "pids": 2048, "disk_bytes": 40 * GIB}


class DockerFixture:
    engine, capacity = "engine", CAPACITY
    info = {"Architecture": "x86_64"}

    def __init__(self):
        self.state, self.removed, self.code, self.owner = "created", False, 0, "mine"
        self.logs, self.calls = "fixture output\n", []

    def command(self, *args, **kwargs):
        self.calls.append(args)
        if args == ("image", "inspect", "image"):
            return json.dumps([{"Id": "image", "Architecture": "amd64"}])
        if args[0] == "run":
            return str(100 * GIB)
        if args[0] == "create":
            self.created = args
            return CID
        if args[0] == "start":
            self.state = "running"
            return CID
        if args[0] == "wait":
            self.state = "exited"
            return str(self.code)
        if args[0] == "logs":
            return self.logs
        if args[:2] == ("container", "rm"):
            if self.state == "running":
                raise Refused("running container cannot be removed without force")
            self.removed = True
            return CID
        if args[:2] == ("container", "ls"):
            return "" if self.removed else CID + "\n"
        raise AssertionError(args)

    def inspect(self, kind, names):
        if kind == "volume":
            return [{"Name": name, "Labels": {API["LABEL"]: self.owner, "io.hsearch.namespace": NAMESPACE}} for name in names]
        info = {"Id": CID, "Image": "image",
                "Config": {"Labels": {API["LABEL"]: self.owner, "io.hsearch.namespace": NAMESPACE}},
                "State": {"Status": self.state, "Running": self.state == "running", "ExitCode": self.code}}
        if hasattr(self, "created"):
            def option(name):
                return self.created[self.created.index(name) + 1]
            info["Config"]["User"] = option("--user")
            info["HostConfig"] = {
                "ReadonlyRootfs": "--read-only" in self.created, "NetworkMode": option("--network"),
                "NanoCpus": int(float(option("--cpus")) * 1_000_000_000),
                "Memory": int(option("--memory")), "MemorySwap": int(option("--memory-swap")),
                "PidsLimit": int(option("--pids-limit"))}
        return [info]


@unittest.skipIf(os.name == "nt", "Temporary resource fixtures are qualified in the Linux image")
class RecoveryControls(unittest.TestCase):
    def test_invalid_artifact_mount_refuses_before_reserving_or_running(self):
        with tempfile.TemporaryDirectory() as directory:
            docker = DockerFixture()
            sandbox = self.ready(directory, docker)
            with self.assertRaisesRegex(Refused, "artifact output escaped"):
                sandbox.run(NAMESPACE, "navigation-build", artifact_directory=Path(directory) / "foreign")
            with sandbox.registry.edit() as data:
                self.assertEqual(set(data["leases"]), {NAMESPACE})
            self.assertEqual(docker.calls, [])

    def test_public_frozen_code_is_readable_by_a_different_guest_uid_under_private_umask(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory) / "harness"
            previous = os.umask(0o077)
            try:
                hashes = API["freeze_harness"](folder)
            finally:
                os.umask(previous)
            self.assertEqual(folder.stat().st_mode & 0o005, 0o005, "Guest needs read/traverse on the mount root")
            for name, digest in hashes.items():
                script = folder / name
                self.assertTrue(stat.S_ISREG(script.stat().st_mode))
                self.assertEqual(script.stat().st_mode & 0o004, 0o004, "Distinct guest UID must read the code")
                self.assertEqual(script.stat().st_mode & 0o222, 0, "Published controller files must not be writable")
                self.assertEqual(hashlib.sha256(script.read_bytes()).hexdigest(), digest)

    def ready(self, directory, docker, source=None, frozen=False):
        sandbox = Sandbox(directory, docker=docker, free_disk=lambda: 100 * GIB, source_root=source)
        record = {"namespace": NAMESPACE, "owner": "mine", "source": str(sandbox.source_root), "image": "image", "phase": "ready"}
        if frozen:
            record["snapshot_schema"] = 2
            record["resource_protocol"] = 2
            folder = sandbox.snapshot(record) / "harness"
            folder.mkdir(parents=True)
            record["harness_hashes"] = {}
            for name in API["HARNESS_FILES"]:
                content = (API["ROOT"] / "dev" / name).read_bytes()
                (folder / name).write_bytes(content)
                record["harness_hashes"][name] = hashlib.sha256(content).hexdigest()
        sandbox.registry.acquire(NAMESPACE, API["STORAGE"], CAPACITY, "engine", record)
        return sandbox

    def child(self, sandbox, *, active=True, pid=999999):
        identity = API["owner_identity"]() if pid == os.getpid() else "departed-worker"
        sandbox.registry.acquire("run-claim", API["BUILD"], CAPACITY, "engine",
            {"kind": "run", "parent": NAMESPACE, "protocol": 2, "phase": "running", "container_id": CID,
             "pid": pid, "process_identity": identity, "owner_active": active})

    def test_success_and_known_failure_remove_exact_id_without_force(self):
        for code in (0, 7):
            with tempfile.TemporaryDirectory() as directory:
                docker = DockerFixture()
                docker.code = code
                sandbox = self.ready(directory, docker)
                if code:
                    with self.assertRaises(API["WorkloadFailed"]) as caught:
                        sandbox.run(NAMESPACE, "check", stream=False)
                    self.assertEqual(caught.exception.result["exit_code"], code)
                else:
                    self.assertEqual(sandbox.run(NAMESPACE, "check", stream=False)["exit_code"], 0)
                self.assertTrue(docker.removed)
                self.assertIn(("container", "rm", CID), docker.calls)
                self.assertFalse(any("--force" in call for call in docker.calls))
                with sandbox.registry.edit() as data:
                    self.assertEqual(set(data["leases"]), {NAMESPACE})

    def test_missing_or_changed_actual_image_refuses_before_start_or_removal(self):
        for operation in ("run", "reconcile"):
            for image in (None, "changed-image"):
                with self.subTest(operation=operation, image=image), tempfile.TemporaryDirectory() as directory:
                    docker = DockerFixture()
                    sandbox = self.ready(directory, docker)
                    if operation == "reconcile":
                        docker.state = "exited"
                        self.child(sandbox, active=False, pid=os.getpid())
                    inspect = docker.inspect

                    def changed(kind, names):
                        info = inspect(kind, names)
                        if kind == "container":
                            info[0]["Config"]["Image"] = "image"  # Must not substitute for actual Image.
                            if image is None:
                                del info[0]["Image"]
                            else:
                                info[0]["Image"] = image
                        return info

                    with patch.object(docker, "inspect", side_effect=changed):
                        with self.assertRaisesRegex(Refused, "actual image changed or missing"):
                            if operation == "run":
                                sandbox.run(NAMESPACE, "check", stream=False)
                            else:
                                sandbox.reconcile(NAMESPACE)
                    self.assertFalse(docker.removed)
                    self.assertFalse(any(call[0] == "start" or call[:2] == ("container", "rm")
                                         for call in docker.calls))
                    with sandbox.registry.edit() as data:
                        self.assertTrue(any(item.get("parent") == NAMESPACE for item in data["leases"].values()))

    def test_success_exports_bounded_actual_public_execution_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            docker = DockerFixture()
            sandbox = self.ready(directory, docker)
            budget = API["BUILD"]
            result = sandbox.run(NAMESPACE, "check", output_budget=4096, stream=False)
            self.assertEqual(result["execution"], {
                "container_id": CID, "image_id": "image", "architecture": "x86_64", "engine": "engine",
                "user": "10001:10001", "read_only": True, "network": "none",
                "limits": {"cpu_nanos": budget["cpu_millis"] * 1_000_000,
                           "memory": budget["memory_bytes"], "memory_swap": budget["memory_bytes"],
                           "pids": budget["pids"]},
                "state": {"status": "exited", "running": False, "exit_code": 0},
                "budget": budget, "output_budget": 4096})
            self.assertIn(("image", "inspect", "image"), docker.calls)
            self.assertFalse(result["truncated"])
            self.assertLessEqual(len(result["output"].encode()), 4096)
            self.assertTrue(docker.removed)

    def test_output_overflow_cannot_be_reported_as_success(self):
        with tempfile.TemporaryDirectory() as directory:
            docker = DockerFixture()
            docker.logs = "é" * 8192
            sandbox = self.ready(directory, docker)
            with self.assertRaises(API["WorkloadFailed"]) as caught:
                sandbox.run(NAMESPACE, "check", output_budget=4096, stream=False)
            self.assertTrue(caught.exception.result["truncated"])
            self.assertLessEqual(len(caught.exception.result["output"].encode()), 4096)

    def test_running_or_foreign_container_cannot_be_reclaimed(self):
        with tempfile.TemporaryDirectory() as directory:
            docker = DockerFixture()
            docker.state = "running"
            sandbox = self.ready(directory, docker)
            self.child(sandbox, active=False, pid=os.getpid())
            with self.assertRaisesRegex(Refused, "still running"):
                sandbox.reconcile(NAMESPACE)
            self.assertFalse(docker.removed)
            docker.state, docker.owner = "exited", "foreign"
            with self.assertRaisesRegex(Refused, "owner does not match"):
                sandbox.reconcile(NAMESPACE)
            self.assertFalse(docker.removed)

    def test_departed_worker_recovers_known_id_but_requires_rerun(self):
        with tempfile.TemporaryDirectory() as directory:
            docker = DockerFixture()
            docker.state = "exited"
            sandbox = self.ready(directory, docker)
            self.child(sandbox)
            real = Sandbox.reconcile.__globals__["owner_identity"]
            with patch.dict(Sandbox.reconcile.__globals__, owner_identity=lambda pid=None: None if pid == 999999 else real(pid)):
                recovered = sandbox.reconcile(NAMESPACE)
            self.assertIn("rerun checks required", recovered[0]["outcome"])
            self.assertTrue(docker.removed)
            with sandbox.registry.edit() as data:
                self.assertEqual(set(data["leases"]), {NAMESPACE})

    def test_live_worker_or_unacknowledged_create_never_reclaimed(self):
        with tempfile.TemporaryDirectory() as directory:
            docker = DockerFixture()
            sandbox = self.ready(directory, docker)
            self.child(sandbox, pid=os.getpid())
            with self.assertRaisesRegex(Refused, "live or uncertain"):
                sandbox.reconcile(NAMESPACE)
            self.assertFalse(docker.calls)
            with sandbox.registry.edit() as data:
                data["leases"]["run-claim"].update(owner_active=False, container_id=None)
            docker.removed = True  # Empty Docker listing still proves nothing about an unknown create.
            with self.assertRaisesRegex(Refused, "Unacknowledged|unacknowledged"):
                sandbox.reconcile(NAMESPACE)
            with sandbox.registry.edit() as data:
                self.assertIn("run-claim", data["leases"])

    def test_worker_source_cannot_replace_frozen_controller_scripts(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "worker"
            (source / "dev").mkdir(parents=True)
            (source / "dev/execute.py").write_text("raise RuntimeError('worker monitor must not run')\n")
            docker = DockerFixture()
            sandbox = self.ready(Path(directory) / "state", docker, source, frozen=True)
            sandbox.run(NAMESPACE, "check", stream=False)
            self.assertIn("/harness/execute.py", docker.created)
            frozen = sandbox.harness(sandbox.registry.get(NAMESPACE))
            self.assertIn(f"type=bind,source={frozen},target=/harness,readonly", docker.created)
            self.assertNotEqual(frozen, source / "dev")
            (frozen / "execute.py").write_text("changed\n")
            with self.assertRaisesRegex(Refused, "Frozen harness script changed"):
                sandbox.run(NAMESPACE, "check", stream=False)

    def test_command_output_budget_stops_its_producer(self):
        execute = runpy.run_path(str(API["ROOT"] / "dev/execute.py"))
        globals_ = execute["run"].__globals__
        children = []
        real_popen = subprocess.Popen

        def spawn(*args, **kwargs):
            child = real_popen(*args, **kwargs)
            children.append(child)
            return child

        with patch.dict(globals_, verify_limits=lambda *args: {}, usage=lambda *args: 0):
            execute["COMMANDS"]["overflow"] = [sys.executable, "-c", "import os,time;os.write(1,b'x'*1000000);time.sleep(10)"]
            with patch.object(subprocess, "Popen", side_effect=spawn):
                with self.assertRaisesRegex(RuntimeError, "output budget"):
                    execute["run"]("overflow", 1024**2, 5, 4096)
        self.assertIsNotNone(children[0].poll())

    def test_unavailable_monitor_isolation_cannot_start_repository_code(self):
        execute = runpy.run_path(str(API["ROOT"] / "dev/execute.py"))
        library = type("RefusedPrctl", (), {"prctl": lambda *args: -1})()
        with patch.dict(execute["run"].__globals__, verify_limits=lambda *args: {}):
            with patch.object(execute["ctypes"], "CDLL", return_value=library), patch.object(subprocess, "Popen") as child:
                with self.assertRaisesRegex(RuntimeError, "isolate the command monitor"):
                    execute["run"]("check", 1024**2, 5)
                child.assert_not_called()

    def test_replaced_foreign_volume_refuses_before_workload_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            docker = DockerFixture()
            sandbox = self.ready(directory, docker, frozen=True)
            docker.owner = "foreign"
            with self.assertRaisesRegex(Refused, "owner does not match"):
                sandbox.run(NAMESPACE, "check", stream=False)
            self.assertFalse(docker.calls)
            with sandbox.registry.edit() as data:
                self.assertEqual(set(data["leases"]), {NAMESPACE})


if __name__ == "__main__":
    unittest.main()
