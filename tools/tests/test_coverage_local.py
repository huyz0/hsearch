"""Extra coverage mounts require exact owned, immutable snapshots before reservation."""
from pathlib import Path
import hashlib
import os
import runpy
import subprocess
import tempfile
import unittest
from unittest.mock import patch

MODULE = runpy.run_path(str(Path(__file__).resolve().parents[1] / "sandbox.py"))
LOCAL = runpy.run_path(str(Path(__file__).resolve().parents[1] / "coverage_local.py"))


class FrozenBundleControls(unittest.TestCase):
    def test_actual_history_output_is_bounded_before_bundle_file_creation(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory).resolve()
            self.assertTrue(parent.is_relative_to(Path(tempfile.gettempdir()).resolve()))
            root = parent / "repo"; root.mkdir()
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.PIPE, timeout=30)
            git("init", "-q"); git("config", "user.name", "bundle control")
            git("config", "user.email", "fixture@example.invalid"); git("config", "core.autocrlf", "false")
            for index in range(3):
                (root / "current.bin").write_bytes(os.urandom(32 * 1024))
                git("add", "current.bin"); git("commit", "-qm", str(index))
            output = parent / "bounded.bundle"
            with patch.dict(LOCAL["freeze_bundle"].__globals__, ROOT=root):
                with self.assertRaisesRegex(ValueError, "git_output_limit"):
                    LOCAL["freeze_bundle"](output, limit=64 * 1024)
                self.assertFalse(output.exists())
                LOCAL["freeze_bundle"](output, limit=256 * 1024)
                self.assertLessEqual(output.stat().st_size, 256 * 1024)
                git("bundle", "verify", str(output))
                before = output.read_bytes()
                with self.assertRaisesRegex(ValueError, "existing"):
                    LOCAL["freeze_bundle"](output)
                self.assertEqual(output.read_bytes(), before)


class CoverageMountControls(unittest.TestCase):
    def test_invalid_or_unowned_snapshots_never_launch_a_workload(self):
        for change in ("missing", "tampered", "bool_size", "outside_output"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as directory:
                class Docker:
                    engine = "owned-test-engine"
                    capacity = {"cpu_millis": 8000, "memory_bytes": 16 * MODULE["GIB"], "pids": 2048, "disk_bytes": 40 * MODULE["GIB"]}
                    def command(self, *args, **kwargs): raise AssertionError("Invalid source must not reach Docker")
                sandbox = MODULE["Sandbox"](directory, docker=Docker(), free_disk=lambda: 100 * MODULE["GIB"])
                namespace = "hsearch-coverage-0123456789ab"
                record = {"namespace": namespace, "kind": "topology", "source": str(MODULE["ROOT"]), "phase": "ready", "snapshot_schema": 2}
                sandbox.registry.acquire(namespace, MODULE["STORAGE"], Docker.capacity, Docker.engine, record)
                snapshot = sandbox.snapshot(record)
                snapshot.mkdir(parents=True)
                pins = {}
                for name in ("coverage-source.bundle", "coverage-source.json"):
                    content = b"original owned snapshot fixture"
                    (snapshot / name).write_bytes(content)
                    pins[name] = {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()}
                if change == "missing": pins.clear()
                if change == "tampered": (snapshot / "coverage-source.bundle").write_bytes(b"modified owned snapshot fixture")
                if change == "bool_size": pins["coverage-source.bundle"]["bytes"] = True
                sandbox.registry.update(namespace, coverage_inputs=pins)
                output = Path(directory) / "unowned" if change == "outside_output" else snapshot / "coverage-evidence"
                with self.assertRaises(MODULE["Refused"]): sandbox.run(namespace, "coverage", artifact_directory=output)
                with sandbox.registry.edit() as data: self.assertEqual(set(data["leases"]), {namespace})
