"""Semantic ownership and uncertain-outcome controls using an independent Docker model."""
from contextlib import contextmanager
from pathlib import Path
import copy
import json
import os
import runpy
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
API = runpy.run_path(str(ROOT / "tools/navigation.py"))
BASE = runpy.run_path(str(ROOT / "tools/sandbox.py"))
Refused = BASE["Refused"]
CID = "a" * 64
NAMESPACE = "hsearch-navigation-0123456789ab"


class DockerModel:
    engine = "engine"

    def __init__(self):
        self.calls, self.containers, self.volumes = [], {}, {}
        self.reply, self.failure = '{"status":"semantic","data":{}}', None

    def inspect(self, kind, names):
        return [copy.deepcopy((self.containers if kind == "container" else self.volumes)[name]) for name in names]

    def command(self, *args, **_kwargs):
        self.calls.append(args)
        if args[0] == "create":
            if self.failure:
                raise Refused(self.failure)
            return CID
        if args[0] == "exec":
            if self.failure:
                raise Refused(self.failure)
            return self.reply
        if args[:2] == ("container", "ls"):
            identity = next((part[3:] for part in args if part.startswith("id=")), None)
            return "\n".join(key for key in self.containers if identity is None or key == identity)
        if args[0] == "kill":
            self.containers[args[-1]]["State"]["Running"] = False
            return args[-1]
        if args[0] == "wait":
            return "0"
        if args[:2] == ("container", "rm"):
            if self.containers[args[-1]]["State"]["Running"]:
                raise Refused("still running")
            del self.containers[args[-1]]
            return args[-1]
        if args[:2] == ("volume", "ls"):
            return "\n".join(self.volumes)
        if args[:2] == ("volume", "rm"):
            for name in args[2:]:
                del self.volumes[name]
            return "\n".join(args[2:])
        raise AssertionError(args)


class LifecycleControls(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.registry = BASE["Registry"](self.root, free_disk=lambda: 100 * BASE["GIB"])
        self.docker = DockerModel()
        self.record = {"namespace": NAMESPACE, "owner": "mine", "kind": "navigation", "task_claim": "claim",
                       "source": str(self.root), "source_digest": "b" * 64, "owner_session": "session", "engine": "engine",
                       "preflight_complete": True, "volumes_complete": True, "create_pending": False,
                       "container_id": CID, "prefetch_container_id": None, "query_pending": None, "phase": "ready",
                       "image": "image", "lsp_sha256": "c" * 64, "reserve_bytes": 18 * BASE["GIB"],
                       "harness": str(self.root / "harness"), "harness_hashes": {}}
        (self.root / "harness").mkdir()
        capacity = {"cpu_millis": 8000, "memory_bytes": 16 * BASE["GIB"], "pids": 2048, "disk_bytes": 40 * BASE["GIB"]}
        self.registry.acquire(NAMESPACE, BASE["BUILD"], capacity, "engine", self.record)
        labels = {BASE["LABEL"]: "mine", "io.hsearch.namespace": NAMESPACE}
        self.docker.containers[CID] = {"Id": CID, "Config": {"Labels": labels}, "State": {"Running": True}}
        for suffix in ("cargo", "target"):
            name = NAMESPACE + "-" + suffix
            self.docker.volumes[name] = {"Name": name, "Labels": dict(labels)}
        nav = API["Navigation"].__new__(API["Navigation"])
        nav.registry, nav.docker, nav.helpers = self.registry, self.docker, BASE
        nav.need, nav.Refused = BASE["need"], Refused
        nav.claim, nav.owner, nav.source = "claim", "session", {"digest": "b" * 64}
        nav.task = {"worktree": str(self.root)}
        nav.sandbox = SimpleNamespace(run_container=lambda record, cid: self.docker.inspect("container", [cid])[0])
        self.nav = nav

    def tearDown(self):
        self.temporary.cleanup()

    def test_known_create_is_recorded_before_inspection_failure(self):
        def failed(_record, _cid):
            raise Refused("inspect uncertain")
        self.nav.inspect = failed
        self.registry.update(NAMESPACE, create_pending=True, container_id=None)
        with self.assertRaisesRegex(Refused, "inspect uncertain"):
            self.nav.create(self.record, NAMESPACE + "-server", ["controller"], network="none")
        self.assertEqual(self.registry.get(NAMESPACE)["container_id"], CID)
        self.assertFalse(self.registry.get(NAMESPACE)["create_pending"])

    def test_unacknowledged_create_retains_pending_lease_and_blocks_cleanup(self):
        self.registry.update(NAMESPACE, create_pending=True, container_id=None)
        self.docker.failure = "unknown create"
        with self.assertRaisesRegex(Refused, "unknown create"):
            self.nav.create(self.record, NAMESPACE + "-server", ["controller"], network="none")
        self.docker.containers.clear()
        with self.assertRaisesRegex(Refused, "setup outcome uncertain"):
            self.nav.close()
        self.assertTrue(self.registry.get(NAMESPACE)["create_pending"])
        self.assertFalse(any(call[:2] == ("volume", "rm") for call in self.docker.calls))

    def test_query_unknown_outcome_is_not_retried(self):
        self.docker.failure = "exec acknowledgement lost"
        with self.assertRaisesRegex(Refused, "acknowledgement"):
            self.nav.query({"operation": "doctor"})
        self.assertIsNotNone(self.registry.get(NAMESPACE)["query_pending"])
        with self.assertRaisesRegex(Refused, "pending/uncertain"):
            self.nav.query({"operation": "doctor"})
        self.assertEqual(sum(call[0] == "exec" for call in self.docker.calls), 1)

    def test_acknowledged_unavailable_response_clears_query_token(self):
        self.docker.reply = '{"status":"unavailable","reason":"missing tool"}'
        self.assertEqual(self.nav.query({"operation": "doctor"})["status"], "unavailable")
        self.assertIsNone(self.registry.get(NAMESPACE)["query_pending"])

    def test_stale_session_does_not_query_or_implicitly_restart(self):
        self.nav.source = {"digest": "d" * 64}
        self.assertEqual(self.nav.query({"operation": "doctor"})["status"], "unavailable")
        self.assertEqual(self.docker.calls, [])

    def test_known_empty_low_space_preflight_releases_but_unknown_probe_retains(self):
        self.registry.release(NAMESPACE)
        self.nav.image = lambda: {"id": "image", "navigation_artifact": {"binary_sha256": "c" * 64}}
        self.nav.sandbox.namespace = lambda _kind: NAMESPACE
        self.docker.capacity = {"cpu_millis": 3000, "memory_bytes": 8 * BASE["GIB"], "pids": 2048, "disk_bytes": 40 * BASE["GIB"]}
        self.nav.sandbox.backend_free = lambda *_args: 0
        def refuses(_free):
            raise Refused("low backend reserve")
        self.nav.sandbox.require_backend_reserve = refuses
        with self.assertRaisesRegex(Refused, "low backend"):
            self.nav.start()
        with self.assertRaisesRegex(Refused, "Unknown"):
            self.registry.get(NAMESPACE)
        def unknown(*_args):
            raise Refused("probe outcome unknown")
        self.nav.sandbox.backend_free = unknown
        with self.assertRaisesRegex(Refused, "probe outcome unknown"):
            self.nav.start()
        self.assertEqual(self.registry.get(NAMESPACE)["phase"], "preflight")

    def test_foreign_cache_volume_refuses_queries_and_is_never_removed(self):
        self.docker.volumes[NAMESPACE + "-target"]["Labels"][BASE["LABEL"]] = "someone-else"
        with self.assertRaisesRegex(Refused, "owner does not match"):
            self.nav.query({"operation": "doctor"})
        with self.assertRaisesRegex(Refused, "owner does not match"):
            self.nav.close()
        self.registry.get(NAMESPACE)
        self.assertFalse(any(call[:2] == ("volume", "rm") for call in self.docker.calls))

    def test_missing_cache_volume_refuses_before_query(self):
        del self.docker.volumes[NAMESPACE + "-target"]
        with self.assertRaises(KeyError):
            self.nav.query({"operation": "doctor"})
        self.assertEqual(self.docker.calls, [])

    def test_controller_snapshot_change_refuses_before_query(self):
        (self.root / "harness" / "injected.py").write_text("changed")
        with self.assertRaisesRegex(Refused, "snapshot changed"):
            self.nav.query({"operation": "doctor"})
        self.assertEqual(self.docker.calls, [])

    def test_known_running_controller_stops_without_force_before_release(self):
        self.assertEqual(self.nav.close()["status"], "stopped")
        self.assertIn(("kill", "--signal", "TERM", CID), self.docker.calls)
        self.assertFalse(any("--force" in call for call in self.docker.calls))
        self.assertEqual(self.docker.containers, {})
        self.assertEqual(self.docker.volumes, {})
        with self.assertRaisesRegex(Refused, "Unknown"):
            self.registry.get(NAMESPACE)

    def test_foreign_task_or_engine_cannot_adopt_session(self):
        for field, value in (("owner_session", "other"), ("source", "other"), ("engine", "other")):
            with self.subTest(field=field):
                self.registry.update(NAMESPACE, **{field: value})
                with self.assertRaisesRegex(Refused, "ownership changed"):
                    self.nav.existing()
                self.registry.update(NAMESPACE, **{field: self.record[field]})

    def test_exclusive_busy_and_failed_lifecycle_preserves_recovery_state(self):
        task = {"phase": "running", "worker": None}
        @contextmanager
        def edit():
            yield {"tasks": {"claim": task}}
        self.nav.store = SimpleNamespace(edit=edit)
        self.nav.api = {"handle_live": lambda _worker: False}
        with self.assertRaisesRegex(Refused, "busy"):
            with self.nav.exclusive():
                self.fail("busy lifecycle admitted")
        task["phase"] = "allocated"
        with self.assertRaisesRegex(RuntimeError, "interrupted"):
            with self.nav.exclusive():
                self.assertEqual(task["phase"], "navigating")
                raise RuntimeError("interrupted")
        self.assertEqual(task["phase"], "interrupted")
        self.assertIsNone(task["worker"])


@unittest.skipUnless(os.name == "posix", "Frozen guest process controls require Linux")
class GuestControls(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        sys.path.insert(0, str(ROOT / "dev"))
        try:
            cls.guest = runpy.run_path(str(ROOT / "dev/navigation.py"))
        finally:
            sys.path.pop(0)

    def test_combined_output_and_deadline_are_enforced(self):
        with self.assertRaisesRegex(RuntimeError, "output_limit"):
            self.guest["bounded"]([sys.executable, "-c", "import sys; sys.stdout.write('x'*800); sys.stderr.write('x'*800)"], limit=1024)
        with self.assertRaisesRegex(RuntimeError, "deadline"):
            self.guest["bounded"]([sys.executable, "-c", "import time; time.sleep(10)"], timeout=0.1)

    def test_untrusted_health_state_rejects_fifo_symlink_and_oversized_regular_file(self):
        with tempfile.TemporaryDirectory() as folder, patch.dict(self.guest["read_ready"].__globals__, STATE=Path(folder)):
            path = Path(folder) / "ready.json"
            os.mkfifo(path)
            with self.assertRaisesRegex(RuntimeError, "limit_or_type"):
                self.guest["read_ready"]()
            path.unlink()
            path.symlink_to(Path(folder) / "other")
            with self.assertRaises(OSError):
                self.guest["read_ready"]()
            path.unlink()
            path.write_text("x" * 32769)
            with self.assertRaisesRegex(RuntimeError, "limit_or_type"):
                self.guest["read_ready"]()

    def test_artifact_mount_is_charged_to_the_frozen_build_monitor(self):
        code = """import os,runpy,sys
m=runpy.run_path(sys.argv[1]); g=m['run'].__globals__
g['verify_limits']=lambda *args: {}
g['usage']=lambda paths: 2048 if '/navigation-output' in paths else 0
g['COMMANDS']['navigation-build']=[sys.executable,'-c','import time; time.sleep(0.2)']
os.environ.pop('HSEARCH_BACKEND_RESERVE_BYTES',None)
try: m['run']('navigation-build',1024,3,4096)
except RuntimeError as error: print(str(error))
"""
        result = subprocess.check_output([sys.executable, "-c", code, str(ROOT / "dev/execute.py")], text=True, timeout=10)
        self.assertIn("cache/target disk budget exceeded", result)

    def test_dead_cli_with_descendant_holding_pipes_cannot_escape_deadline_cleanup(self):
        with tempfile.TemporaryDirectory() as folder:
            pid_file = Path(folder) / "pid"
            code = "import subprocess,sys; from pathlib import Path; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); Path(sys.argv[1]).write_text(str(p.pid))"
            with self.assertRaisesRegex(RuntimeError, "deadline"):
                self.guest["bounded"]([sys.executable, "-c", code, str(pid_file)], timeout=0.5)
            pid = int(pid_file.read_text())
            for _ in range(20):
                path = Path(f"/proc/{pid}/stat")
                if not path.exists() or path.read_text().rsplit(")", 1)[1].split()[0] == "Z":
                    break
                time.sleep(0.05)
            else:
                self.fail("Owned query descendant outlived deadline cleanup")


if __name__ == "__main__":
    unittest.main()
