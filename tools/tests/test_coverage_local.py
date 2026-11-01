"""Extra coverage mounts require exact owned, immutable snapshots before reservation."""
from pathlib import Path
import hashlib
import runpy
import tempfile
import unittest

MODULE = runpy.run_path(str(Path(__file__).resolve().parents[1] / "sandbox.py"))


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
