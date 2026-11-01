"""Resource refusal and ownership controls, without touching ambient Docker resources."""
from pathlib import Path
import copy
import json
from functools import partial
import os
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

MODULE = runpy.run_path(str(Path(__file__).resolve().parents[1] / "sandbox.py"))
Refused = MODULE["Refused"]
BUILD, STORAGE, GIB = (MODULE[k] for k in ("BUILD", "STORAGE", "GIB"))
# Admission sensor fixtures are independent of the test process's small tmpfs.
Registry = partial(MODULE["Registry"], free_disk=lambda: 100 * GIB)
Sandbox = partial(MODULE["Sandbox"], free_disk=lambda: 100 * GIB)
CAPACITY = {"cpu_millis": 8000, "memory_bytes": 16 * GIB, "pids": 2048, "disk_bytes": 40 * GIB}


class AdmissionControls(unittest.TestCase):
    def test_two_bounded_builds_fit_but_overcommit_fails(self):
        leases = [{"budget": BUILD}]
        MODULE["admit"](BUILD, leases, CAPACITY, 100 * GIB)
        for key in BUILD:
            request = dict(BUILD)
            request[key] = CAPACITY[key]
            with self.assertRaisesRegex(Refused, "budget exhausted"):
                MODULE["admit"](request, leases, CAPACITY, 100 * GIB)

    def test_low_disk_and_invalid_values_refuse_before_allocation(self):
        with self.assertRaisesRegex(Refused, "Low host disk"):
            MODULE["admit"](BUILD, [], CAPACITY, 2 * GIB)
        for invalid in (0, -1, True, 1.5):
            request = dict(BUILD, cpu_millis=invalid)
            with self.assertRaisesRegex(Refused, "Invalid"):
                MODULE["admit"](request, [], CAPACITY, 100 * GIB)

    def test_disk_headroom_includes_outstanding_unconsumed_reservations(self):
        MODULE["admit"](STORAGE, [], CAPACITY, 20 * GIB)
        with self.assertRaisesRegex(Refused, "Low host disk"):
            MODULE["admit"](STORAGE, [{"budget": STORAGE}], CAPACITY, 20 * GIB)
        MODULE["admit"](STORAGE, [{"budget": STORAGE}], CAPACITY, 30 * GIB)

    def test_both_topologies_and_probe_fit_small_native_ci_capacity(self):
        small = dict(CAPACITY, cpu_millis=3000, memory_bytes=8 * GIB)
        MODULE["admit"](MODULE["PROBE"], [{"budget": STORAGE}, {"budget": STORAGE}], small, 100 * GIB)


class RegistryControls(unittest.TestCase):
    def test_reservations_persist_and_are_not_reclaimed_by_age(self):
        with tempfile.TemporaryDirectory() as directory:
            registry = Registry(directory)
            registry.acquire("first", BUILD, CAPACITY, "engine")
            again = Registry(directory)
            self.assertEqual(again.get("first")["budget"], BUILD)
            with self.assertRaisesRegex(Refused, "already exists"):
                again.acquire("first", BUILD, CAPACITY, "engine")
            with self.assertRaisesRegex(Refused, "different engine"):
                again.acquire("second", BUILD, CAPACITY, "another")
            again.release("first")
            with self.assertRaisesRegex(Refused, "Unknown"):
                registry.get("first")

    def test_kernel_lock_prevents_concurrent_oversubscription(self):
        with tempfile.TemporaryDirectory() as directory:
            script = str(Path(MODULE["ROOT"]) / "tools/sandbox.py")
            worker = "import runpy,sys; m=runpy.run_path(sys.argv[1]); r=m['Registry'](sys.argv[2],free_disk=lambda:100*m['GIB']); c=dict(m['BUILD']); r.acquire(sys.argv[3],m['BUILD'],c,'engine')"
            children = [subprocess.Popen([sys.executable, "-c", worker, script, directory, f"worker-{i}"], stdout=subprocess.PIPE, stderr=subprocess.PIPE) for i in range(2)]
            outcomes = [child.communicate(timeout=15) for child in children]
            self.assertEqual(sorted(child.returncode for child in children), [0, 1], outcomes)
            with Registry(directory).edit() as data:
                self.assertEqual(len(data["leases"]), 1)

    def test_process_identity_is_real_and_distinguishes_exit(self):
        self.assertNotIn(MODULE["owner_identity"](), (None, "unknown"))
        child = subprocess.Popen([sys.executable, "-c", "input()"], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL)
        try:
            token = MODULE["owner_identity"](child.pid)
            self.assertNotIn(token, (None, "unknown"))
            child.communicate(b"\n", timeout=10)
            self.assertIsNone(MODULE["owner_identity"](child.pid))
        finally:
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=5)


class OwnershipControls(unittest.TestCase):
    def test_foreign_resources_cannot_pass_cleanup_guard(self):
        MODULE["assert_owned"]([{"Labels": {MODULE["LABEL"]: "mine"}}], "mine")
        for item in ({"Labels": None}, {"Labels": {MODULE["LABEL"]: "other"}}, {"Config": {"Labels": {}}}):
            with self.assertRaisesRegex(Refused, "owner does not match"):
                MODULE["assert_owned"]([item], "mine")

    def test_paths_and_task_names_are_bounded(self):
        for namespace in ("..", "../../other", "hsearch-foo", "hsearch-foo-0123456789ab/../other"):
            with self.assertRaisesRegex(Refused, "Invalid resource namespace"):
                MODULE["owned_snapshot"]({"namespace": namespace})
        with tempfile.TemporaryDirectory() as directory:
            sandbox = Sandbox(directory, docker=object())
            for name in ("../other", "a;touch x", "a" * 26):
                with self.assertRaisesRegex(Refused, "Invalid task"):
                    sandbox.namespace(name)

    def test_engine_down_is_a_refusal_not_a_silent_mock(self):
        with patch.object(subprocess, "run", side_effect=FileNotFoundError):
            with self.assertRaisesRegex(Refused, "Docker unavailable"):
                MODULE["Docker"]()

    def test_backend_low_disk_refuses_topology_and_releases_known_empty_lease(self):
        class FakeDocker:
            engine = "engine"
            capacity = CAPACITY
            free = 0
            def command(self, *args, **kwargs):
                if args[0] != "run":
                    raise AssertionError("No topology mutations allowed after low disk")
                self.probe = args
                return str(self.free) + "\n"
        for free, prior in ((0, False), (20 * GIB, True)):
            with tempfile.TemporaryDirectory() as directory:
                docker = FakeDocker()
                docker.free = free
                sandbox = Sandbox(directory, docker=docker)
                digest = MODULE["hashlib"].sha256((Path(MODULE["ROOT"]) / "dev/Dockerfile").read_bytes()).hexdigest()
                (Path(directory) / "images").mkdir()
                MODULE["atomic_json"](Path(directory) / "images" / (digest + ".json"), {"id": "image", "engine": "engine", "dockerfile_sha256": digest})
                if prior:
                    sandbox.registry.acquire("prior", STORAGE, CAPACITY, "engine")
                with self.assertRaisesRegex(Refused, "backend disk reserve"):
                    sandbox.up("test")
                self.assertIn("--read-only", docker.probe)
                with sandbox.registry.edit() as data:
                    self.assertEqual(set(data["leases"]), {"prior"} if prior else set())

    def test_down_refuses_foreign_container_before_mutation(self):
        class FakeDocker:
            engine = "engine"
            def __init__(self):
                self.calls = []
            def command(self, *args, **kwargs):
                self.calls.append(args)
                return "cid\n"
            def inspect(self, kind, names):
                return [{"Id": "cid", "Config": {"Labels": {MODULE["LABEL"]: "foreign"}}}]
        with tempfile.TemporaryDirectory() as directory:
            docker = FakeDocker()
            sandbox = Sandbox(directory, docker=docker)
            namespace = "hsearch-test-0123456789ab"
            sandbox.registry.acquire(namespace, STORAGE, CAPACITY, "engine", {"namespace": namespace, "owner": "mine", "source": str(MODULE["ROOT"]), "phase": "ready"})
            with self.assertRaisesRegex(Refused, "owner does not match"):
                sandbox.down(namespace)
            self.assertTrue(all(call[:2] == ("container", "ls") for call in docker.calls))

    def test_down_refuses_live_setup_and_concurrent_cleanup(self):
        class FakeDocker:
            engine = "engine"
            def command(self, *args, **kwargs):
                raise AssertionError("A live lifecycle owner must be checked before Docker calls")
        for phase in ("starting", "stopping"):
            with tempfile.TemporaryDirectory() as directory:
                sandbox = Sandbox(directory, docker=FakeDocker())
                namespace = "hsearch-test-0123456789ab"
                sandbox.registry.acquire(namespace, STORAGE, CAPACITY, "engine", {"namespace": namespace, "owner": "mine", "source": str(MODULE["ROOT"]), "phase": phase})
                with self.assertRaisesRegex(Refused, "lifecycle owner"):
                    sandbox.down(namespace)

    def test_unknown_run_outcome_keeps_lease_and_blocks_cleanup(self):
        class FakeDocker:
            engine = "engine"
            capacity = CAPACITY
            def command(self, *args, **kwargs):
                if args[0] == "run":
                    if any(str(arg).endswith("-admission") for arg in args):
                        return str(100 * GIB)
                    raise Refused("unknown run outcome")
                return "live-cid\n"
        with tempfile.TemporaryDirectory() as directory:
            sandbox = Sandbox(directory, docker=FakeDocker())
            namespace = "hsearch-test-0123456789ab"
            sandbox.registry.acquire(namespace, STORAGE, CAPACITY, "engine", {"namespace": namespace, "owner": "mine", "source": str(MODULE["ROOT"]), "image": "image", "phase": "ready"})
            with self.assertRaisesRegex(Refused, "unknown run"):
                sandbox.run(namespace, "probe")
            with self.assertRaisesRegex(Refused, "uncertain run lease"):
                sandbox.down(namespace)

    def test_unknown_create_keeps_lease_even_when_no_container_is_visible(self):
        class FakeDocker:
            engine = "engine"
            capacity = CAPACITY
            def command(self, *args, **kwargs):
                if args[0] == "run":
                    if any(str(arg).endswith("-admission") for arg in args):
                        return str(100 * GIB)
                    raise Refused("unknown create result")
                return ""
        with tempfile.TemporaryDirectory() as directory:
            sandbox = Sandbox(directory, docker=FakeDocker())
            namespace = "hsearch-test-0123456789ab"
            sandbox.registry.acquire(namespace, STORAGE, CAPACITY, "engine", {"namespace": namespace, "owner": "mine", "source": str(MODULE["ROOT"]), "image": "image", "phase": "ready"})
            with self.assertRaisesRegex(Refused, "unknown create"):
                sandbox.run(namespace, "probe")
            with sandbox.registry.edit() as data:
                self.assertEqual(sum(x.get("parent") == namespace for x in data["leases"].values()), 1)
            with self.assertRaisesRegex(Refused, "uncertain run lease"):
                sandbox.down(namespace)

    def test_run_rechecks_backend_after_topology_start_before_workload(self):
        class FakeDocker:
            engine = "engine"
            capacity = CAPACITY
            def command(self, *args, **kwargs):
                if args[0] == "run" and any(str(arg).endswith("-admission") for arg in args):
                    for suffix in ("cargo", "target", "data"):
                        if not any(f"source=hsearch-test-0123456789ab-{suffix}," in str(arg) and "readonly,volume-nocopy" in str(arg) for arg in args):
                            raise AssertionError("Backend admission must sample actual owned data volumes read-only")
                    return str(5 * GIB)
                raise AssertionError("Workload must not start when backend free space has fallen")
        with tempfile.TemporaryDirectory() as directory:
            sandbox = Sandbox(directory, docker=FakeDocker())
            namespace = "hsearch-test-0123456789ab"
            sandbox.registry.acquire(namespace, STORAGE, CAPACITY, "engine", {"namespace": namespace, "owner": "mine", "source": str(MODULE["ROOT"]), "image": "image", "phase": "ready"})
            with self.assertRaisesRegex(Refused, "backend disk reserve"):
                sandbox.run(namespace, "check")
            with sandbox.registry.edit() as data:
                self.assertEqual(set(data["leases"]), {namespace})
                self.assertEqual(data["leases"][namespace]["phase"], "ready")

    def test_compose_snapshot_has_no_host_socket_or_static_secret(self):
        template = (Path(MODULE["ROOT"]) / "dev/compose.json").read_text()
        for forbidden in ("docker.sock", "privileged", "rustfsadmin", "latest"):
            self.assertNotIn(forbidden, template)
        from string import Template
        parsed = json.loads(Template(template).substitute(DEV_IMAGE="sha256:abc", RUSTFS_IMAGE="rustfs@sha256:def", OWNER="owner", NAMESPACE="test", SOURCE_ROOT="/repo"))
        for service in parsed["services"].values():
            self.assertEqual(service["user"], "10001:10001")
            self.assertTrue(service["read_only"])
            self.assertEqual(service["cap_drop"], ["ALL"])
            self.assertGreater(service["pids_limit"], 0)
        self.assertTrue(parsed["networks"]["default"]["internal"])
        self.assertNotIn("ports", parsed["services"]["rustfs"])


class ExecutionControls(unittest.TestCase):
    @unittest.skipIf(os.name == "nt", "Process groups are enforced inside the Linux dev container")
    def test_disk_and_deadline_abort_only_the_owned_process_group(self):
        execute = runpy.run_path(str(Path(MODULE["ROOT"]) / "dev/execute.py"))
        globals_ = execute["run"].__globals__
        globals_["COMMANDS"] = {"control": [sys.executable, "-c", "import time; time.sleep(60)"]}
        globals_["verify_limits"] = lambda *args: {}  # Separate process-group behavior from host sensor fixtures.
        real_popen = subprocess.Popen
        bystander = real_popen([sys.executable, "-c", "import time; time.sleep(60)"], start_new_session=True)
        try:
            for used, limit, deadline, message in ((2, 1, 5, "disk budget"), (0, 1, 0.01, "deadline")):
                children = []
                def spawn(*args, **kwargs):
                    child = real_popen(*args, **kwargs)
                    children.append(child)
                    return child
                globals_["usage"] = lambda *args: used
                with patch.object(subprocess, "Popen", side_effect=spawn):
                    with self.assertRaisesRegex(RuntimeError, message):
                        execute["run"]("control", limit, deadline)
                self.assertIsNotNone(children[0].poll())
                self.assertIsNone(bystander.poll())
        finally:
            bystander.terminate()
            bystander.wait(timeout=5)

    def test_disk_accounting_does_not_follow_symlinks_and_is_bounded(self):
        execute = runpy.run_path(str(Path(MODULE["ROOT"]) / "dev/execute.py"))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "payload").write_bytes(b"x" * 100)
            self.assertGreaterEqual(execute["usage"]([path]), 100)
            with self.assertRaisesRegex(RuntimeError, "entry bound"):
                execute["usage"]([path], max_entries=0)

    @unittest.skipIf(os.name == "nt", "Linux container filesystem is the symlink target")
    def test_disk_scanner_does_not_follow_a_parent_symlink(self):
        execute = runpy.run_path(str(Path(MODULE["ROOT"]) / "dev/execute.py"))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "loop").symlink_to(path, target_is_directory=True)
            self.assertLess(execute["usage"]([path], max_entries=2), 8192)


class ResourceCapabilityControls(unittest.TestCase):
    def test_engine_rejects_missing_resource_delegation(self):
        info = {"OSType": "linux", "CgroupVersion": "2", "CgroupDriver": "systemd", "ID": "engine", "NCPU": 4,
                "MemTotal": 16 * GIB, "MemoryLimit": True, "SwapLimit": True, "CpuCfsPeriod": True, "CpuCfsQuota": True, "PidsLimit": True}
        def invoke(args, **kwargs):
            if args[-2:] == ["context", "show"]:
                output = "default\n"
            elif "inspect" in args:
                output = json.dumps([{"Endpoints": {"docker": {"Host": "unix:///var/run/docker.sock"}}}])
            else:
                output = json.dumps(info)
            return subprocess.CompletedProcess(args, 0, output, "")
        with patch.object(subprocess, "run", side_effect=invoke):
            self.assertEqual(MODULE["Docker"]().engine, "engine")
        for key, value in (("CgroupDriver", "none"), ("MemoryLimit", False), ("SwapLimit", False), ("CpuCfsPeriod", False), ("CpuCfsQuota", False), ("PidsLimit", False)):
            broken = dict(info, **{key: value})
            with patch.object(subprocess, "run", side_effect=lambda args, **kwargs: subprocess.CompletedProcess(args, 0,
                    "default\n" if args[-2:] == ["context", "show"] else json.dumps([{"Endpoints": {"docker": {"Host": "unix:///var/run/docker.sock"}}}]) if "inspect" in args else json.dumps(broken), "")):
                with self.assertRaisesRegex(Refused, "controllers unavailable"):
                    MODULE["Docker"]()

    def test_effective_caps_must_be_finite_and_within_each_declared_bound(self):
        verify = runpy.run_path(str(Path(MODULE["ROOT"]) / "dev/execute.py"))["verify_limits"]
        baseline = {"memory.max": str(4 * GIB), "memory.swap.max": "0", "cpu.max": "200000 100000", "pids.max": "512"}
        self.assertEqual(verify(2000, 4 * GIB, 512, limits=baseline, uid=10001), baseline)
        for key, value in (("memory.max", "max"), ("memory.max", str(8 * GIB)), ("memory.swap.max", "max"), ("memory.swap.max", "1"), ("cpu.max", "max 100000"),
                           ("cpu.max", "400000 100000"), ("pids.max", "max"), ("pids.max", "1024")):
            with self.assertRaisesRegex(RuntimeError, "Effective cgroup caps"):
                verify(2000, 4 * GIB, 512, limits=dict(baseline, **{key: value}), uid=10001)
        with self.assertRaisesRegex(RuntimeError, "non-root"):
            verify(2000, 4 * GIB, 512, limits=baseline, uid=0)

    def test_diagnostics_rejects_the_all_unlimited_counterexample(self):
        execute = runpy.run_path(str(Path(MODULE["ROOT"]) / "dev/execute.py"))
        def sensor(path, *args, **kwargs):
            return "max 100000" if path.name == "cpu.max" else "max"
        with patch.object(os, "getuid", return_value=10001, create=True), \
             patch.object(subprocess, "check_output", side_effect=lambda command, **kwargs: "rustc 1.97.1 fixture" if command[0] == "rustc" else "fixture"), \
             patch.object(Path, "read_text", sensor):
            with self.assertRaisesRegex(RuntimeError, "Effective cgroup caps"):
                execute["diagnostics"]()

    def test_repository_command_cannot_start_before_effective_cap_verification(self):
        execute = runpy.run_path(str(Path(MODULE["ROOT"]) / "dev/execute.py"))
        def refuse(*args):
            raise RuntimeError("counterexample: unbounded")
        execute["run"].__globals__["verify_limits"] = refuse
        with patch.object(subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(RuntimeError, "unbounded"):
                execute["run"]("check", 1, 1)
            spawn.assert_not_called()


if __name__ == "__main__":
    unittest.main()
