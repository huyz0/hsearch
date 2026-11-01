"""Independent task-packet, real Git ownership and durable evidence controls."""
import copy
import hashlib
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest

MODULE = runpy.run_path(str(Path(__file__).resolve().parents[1] / "task.py"))
TaskStore, Refused = MODULE["TaskStore"], MODULE["Refused"]


def packet(task="M01.03", paths=None, base="a" * 40, owner="fixture-owner"):
    return {"schema_version": 1, "task_id": task, "goal": "Exercise the fixture contract",
            "base_revision": base, "owner_session": owner, "owned_paths": paths or ["a.txt"],
            "assumptions": [], "acceptance": ["Independent outcome recorded"],
            "negative_cases": ["Wrong ownership refuses"], "commands": ["diagnostics", "check", "prefetch", "probe"],
            "output_budget_bytes": 16384, "expected_evidence": ["Exact source and bounded results"]}


class PacketControls(unittest.TestCase):
    def test_exact_version_and_all_required_fields(self):
        MODULE["packet"](packet())
        for field in MODULE["SCHEMA"]["required"]:
            value = packet()
            del value[field]
            with self.subTest(field=field), self.assertRaises(Refused):
                MODULE["packet"](value)
        for key, value in (("schema_version", True), ("base_revision", "HEAD"), ("commands", ["shell"]),
                           ("output_budget_bytes", True), ("output_budget_bytes", 0), ("unexpected", 1)):
            candidate = packet()
            candidate[key] = value
            with self.subTest(key=key), self.assertRaises(Refused):
                MODULE["packet"](candidate)

    def test_path_aliases_credentials_and_case_overlap_refused(self):
        for path in (".", "../a", "/a", "D:/a", "a\\b", "a//b", "a/../b", ".git/config", ".aws/config", ".env", "keys/server.pem"):
            with self.subTest(path=path), self.assertRaises(Refused):
                MODULE["packet"](packet(paths=[path]))
        for paths in (["tools", "tools/task.py"], ["TOOLS", "tools/task.py"]):
            with self.assertRaisesRegex(Refused, "overlapping"):
                MODULE["packet"](packet(paths=paths))

    def test_context_budget_is_bounded_without_dropping_ownership(self):
        value = packet()
        value["assumptions"] = ["x" * 2048] * 20
        with self.assertRaisesRegex(Refused, "context budget"):
            MODULE["packet"](value)


@unittest.skipUnless(os.name == "posix" and shutil.which("git"), "Real ownership fixtures run in the Linux image")
class WorktreeControls(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.area = Path(self.temporary.name)
        self.source = self.area / "source"
        self.source.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Task fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        (self.source / "docs/plan").mkdir(parents=True)
        self.ledger = {"milestones": [{"id": "M00", "status": "done", "tasks": []},
            {"id": "M01", "status": "in_progress", "tasks": [
                {"id": "M01.03", "status": "planned", "depends_on": []},
                {"id": "M01.07", "status": "planned", "depends_on": []},
                {"id": "M01.04", "status": "planned", "depends_on": ["M01.03"]}]}]}
        (self.source / "docs/plan/tasks.json").write_text(json.dumps(self.ledger))
        (self.source / "a.txt").write_text("first\n")
        (self.source / "b.txt").write_text("second\n")
        (self.source / ".gitignore").write_text("*.key\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")
        self.base = self.git("rev-parse", "HEAD").strip()
        self.store = TaskStore(self.area / "state", self.source)

    def tearDown(self):
        self.temporary.cleanup()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.source, stderr=subprocess.PIPE, timeout=20).decode()

    def claim(self, task="M01.03", paths=None, owner="fixture-owner"):
        return self.store.claim(packet(task=task, paths=paths, base=self.base, owner=owner))

    def test_two_isolated_worktrees_and_path_claims(self):
        first = self.claim()
        with self.assertRaisesRegex(Refused, "overlap"):
            self.claim("M01.07", ["A.txt"], "other")
        second = self.claim("M01.07", ["b.txt"], "other")
        a, b = self.store.get(first), self.store.get(second)
        self.assertNotEqual(a["worktree"], b["worktree"])
        self.assertNotEqual(a["branch"], b["branch"])
        self.assertEqual(self.store.fingerprint(a)["head"], self.base)
        self.assertEqual(self.store.fingerprint(b)["head"], self.base)
        self.store.cleanup(first, "fixture-owner", self.area / "resources")
        self.assertTrue(Path(b["worktree"]).is_dir())
        self.assertEqual(self.store.get(first)["phase"], "closed")
        self.assertIn(a["branch"], self.git("branch", "--list"))
        self.assertTrue(self.store.evidence(first, "fixture-owner"))

    def test_dependency_readiness_uses_committed_packet_base(self):
        self.ledger["milestones"][1]["tasks"][0]["status"] = "done"
        (self.source / "docs/plan/tasks.json").write_text(json.dumps(self.ledger))
        with self.assertRaisesRegex(Refused, "Unfinished task dependency"):
            self.claim("M01.04")

    def test_wrong_owner_packet_or_allocation_cannot_be_adopted(self):
        claim = self.claim()
        with self.assertRaisesRegex(Refused, "another owner"):
            self.store.context(claim, "other")
        with self.store.edit() as data:
            data["tasks"][claim]["packet"]["owned_paths"] = ["b.txt"]
        with self.assertRaisesRegex(Refused, "packet content changed"):
            self.store.get(claim)

    def test_dirty_unowned_and_ignored_work_is_preserved(self):
        claim = self.claim()
        record = self.store.get(claim)
        root = Path(record["worktree"])
        (root / "a.txt").write_text("owned edit\n")
        with self.assertRaisesRegex(Refused, "Dirty worktree"):
            self.store.cleanup(claim, "fixture-owner", self.area / "resources")
        self.assertEqual((root / "a.txt").read_text(), "owned edit\n")
        (root / "b.txt").write_text("unowned edit\n")
        with self.assertRaisesRegex(Refused, "outside its task claim"):
            self.store.fingerprint(record)
        (root / "b.txt").write_text("second\n")
        (root / "local.key").write_text("fixture data; do not read\n")
        with self.assertRaisesRegex(Refused, "Ignored worktree inputs"):
            self.store.fingerprint(record)
        self.assertTrue((root / "local.key").exists())

    def test_live_or_unknown_worker_blocks_resume_and_cleanup(self):
        claim = self.claim()
        with self.store.edit() as data:
            data["tasks"][claim].update(phase="running", worker={"pid": os.getpid(), "identity": MODULE["SANDBOX"]["owner_identity"]()})
        with self.assertRaisesRegex(Refused, "live or uncertain"):
            self.store.resume(claim, "fixture-owner", self.area / "resources")
        with self.assertRaisesRegex(Refused, "busy"):
            self.store.cleanup(claim, "fixture-owner", self.area / "resources")

    def test_evidence_content_binding_budget_and_exact_context_bytes(self):
        claim = self.claim()
        identity = self.store.append(claim, "fixture-owner", "note", {"text": "héllo ✨"})
        context = self.store.context(claim, "fixture-owner")
        self.assertEqual(context["context_bytes"], len(MODULE["encoded"](context)))
        self.assertTrue(context["token_count_is_estimate"])
        with self.assertRaisesRegex(Refused, "byte budget"):
            self.store.append(claim, "fixture-owner", "note", {"text": "x" * 16384})
        path = self.store.root / "evidence" / claim / (identity + ".json")
        content = path.read_bytes()
        path.write_bytes(content.replace(b"h", b"z", 1))
        with self.assertRaisesRegex(Refused, "content changed"):
            self.store.evidence(claim, "fixture-owner")

    def test_actual_kernel_lock_rejects_concurrent_overlapping_claims(self):
        script = "import json,runpy,sys; t=runpy.run_path(sys.argv[1]); s=t['TaskStore'](sys.argv[2],sys.argv[3]); s.claim(json.loads(sys.argv[4]))"
        module = str(Path(MODULE["ROOT"]) / "tools/task.py")
        children = []
        for task, owner in (("M01.03", "first"), ("M01.07", "second")):
            value = packet(task=task, base=self.base, owner=owner)
            children.append(subprocess.Popen([sys.executable, "-c", script, module,
                str(self.area / "state"), str(self.source), json.dumps(value)], stdout=subprocess.PIPE, stderr=subprocess.PIPE))
        outcomes = []
        for child in children:
            child.communicate(timeout=30)
            outcomes.append(child.returncode)
        self.assertEqual(sorted(outcomes), [0, 1])
        with self.store.edit() as data:
            self.assertEqual(len(data["tasks"]), 1)


if __name__ == "__main__":
    unittest.main()
