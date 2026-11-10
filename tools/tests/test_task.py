"""Independent task-packet, real Git ownership and durable evidence controls."""
import copy
from contextlib import contextmanager
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
from unittest.mock import patch
from types import SimpleNamespace

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

    def test_semantic_lease_blocks_worktree_cleanup_and_wrong_registry_cannot_hide_it(self):
        claim = self.claim()
        resource = self.area / "resources"
        with self.store.edit() as data:
            data["resource_root"] = str(resource.resolve())
        registry = MODULE["SANDBOX"]["Registry"](resource, free_disk=lambda: 100 * 1024**3)
        budget = MODULE["SANDBOX"]["BUILD"]
        registry.acquire("navigation", budget, dict(budget), "fixture", {"kind": "navigation", "task_claim": claim})
        with self.assertRaisesRegex(Refused, "recorded shared resource registry"):
            self.store.cleanup(claim, "fixture-owner", self.area / "different")
        with self.assertRaisesRegex(Refused, "semantic session"):
            self.store.cleanup(claim, "fixture-owner", resource)
        self.assertEqual(self.store.get(claim)["phase"], "allocated")
        self.assertTrue(Path(self.store.get(claim)["worktree"]).is_dir())
        registry.release("navigation")
        self.store.cleanup(claim, "fixture-owner", resource)

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
        identity = self.store.append(claim, "fixture-owner", "note", {"text": "hÃƒÆ’Ã†â€™Ãƒâ€ Ã¢â‚¬â„¢ÃƒÆ’Ã¢â‚¬Â ÃƒÂ¢Ã¢â€šÂ¬Ã¢â€žÂ¢ÃƒÆ’Ã†â€™ÃƒÂ¢Ã¢â€šÂ¬Ã…Â¡ÃƒÆ’Ã¢â‚¬Å¡Ãƒâ€šÃ‚Â©llo ÃƒÆ’Ã†â€™Ãƒâ€ Ã¢â‚¬â„¢ÃƒÆ’Ã¢â‚¬Å¡Ãƒâ€šÃ‚Â¢ÃƒÆ’Ã†â€™ÃƒÂ¢Ã¢â€šÂ¬Ã‚Â¦ÃƒÆ’Ã‚Â¢ÃƒÂ¢Ã¢â‚¬Å¡Ã‚Â¬Ãƒâ€¦Ã¢â‚¬Å“ÃƒÆ’Ã†â€™ÃƒÂ¢Ã¢â€šÂ¬Ã…Â¡ÃƒÆ’Ã¢â‚¬Å¡Ãƒâ€šÃ‚Â¨"})
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

    def test_hidden_index_flags_cannot_hide_or_delete_dirty_work(self):
        for flag, undo in (("--skip-worktree", "--no-skip-worktree"), ("--assume-unchanged", "--no-assume-unchanged")):
            claim = self.claim()
            record = self.store.get(claim)
            root = Path(record["worktree"])
            subprocess.check_call(["git", "update-index", flag, "b.txt"], cwd=root)
            (root / "b.txt").write_text("unowned work must survive\n")
            with self.subTest(flag=flag), self.assertRaisesRegex(Refused, "Hidden index flags"):
                self.store.fingerprint(record)
            with self.assertRaisesRegex(Refused, "Hidden index flags"):
                self.store.cleanup(claim, "fixture-owner", self.area / "resources")
            self.assertEqual((root / "b.txt").read_text(), "unowned work must survive\n")
            subprocess.check_call(["git", "update-index", undo, "b.txt"], cwd=root)
            (root / "b.txt").write_text("second\n")
            self.store.cleanup(claim, "fixture-owner", self.area / "resources")

    def test_actual_bytes_bind_source_even_when_a_clean_filter_hides_edits(self):
        (self.source / ".gitattributes").write_text("a.txt filter=collapse\n")
        self.git("config", "filter.collapse.clean", "printf 'first\\n'")
        self.git("add", ".gitattributes")
        self.git("commit", "-qm", "clean filter fixture")
        self.base = self.git("rev-parse", "HEAD").strip()
        claim = self.claim()
        record = self.store.get(claim)
        root = Path(record["worktree"])
        before = self.store.fingerprint(record)
        (root / "a.txt").write_text("actual compiler input differs\n")
        self.assertEqual(subprocess.check_output(["git", "diff", "HEAD"], cwd=root), b"")
        after = self.store.fingerprint(record)
        self.assertNotEqual(before["digest"], after["digest"])
        self.assertFalse(after["clean"])
        with self.assertRaisesRegex(Refused, "Dirty worktree"):
            self.store.cleanup(claim, "fixture-owner", self.area / "resources")
        self.assertTrue(root.is_dir())

    def test_source_hashing_has_file_and_aggregate_input_bounds(self):
        claim = self.claim()
        record = self.store.get(claim)
        root = Path(record["worktree"])
        original = self.store.fingerprint(record)
        (root / "a.txt").write_bytes(b"x" * (2 * 1024**2))
        changed = self.store.fingerprint(record)
        self.assertNotEqual(original["digest"], changed["digest"])
        with patch.dict(self.store.fingerprint.__globals__, SOURCE_FILE_BYTES=1024):
            with self.assertRaisesRegex(Refused, "bounded regular"):
                self.store.fingerprint(record)
        (root / "a.txt").write_text("first\n")
        with patch.dict(self.store.fingerprint.__globals__, SOURCE_TOTAL_BYTES=1):
            with self.assertRaisesRegex(Refused, "aggregate byte"):
                self.store.fingerprint(record)

    def test_canonical_aliases_refuse_even_when_not_reported_as_symlinks(self):
        claim = self.claim()
        record = self.store.get(claim)
        root = Path(record["worktree"])
        outside = self.area / "outside.txt"
        outside.write_text("first\n")
        (root / "a.txt").unlink()
        (root / "a.txt").symlink_to(outside)
        # Windows junctions are one alias type not reported by is_symlink.
        # A real redirect with that classification suppressed must still refuse.
        with patch.object(Path, "is_symlink", return_value=False):
            with self.assertRaisesRegex(Refused, "redirected"):
                self.store.fingerprint(record)

    def test_git_pipe_overflow_is_refused_without_unbounded_capture(self):
        (self.source / "a.txt").write_bytes(b"x" * (2 * 1024**2))
        with patch.object(subprocess, "run", side_effect=AssertionError("Unbounded communicate/capture is forbidden")):
            with self.assertRaisesRegex(Refused, "output exceeds bound"):
                MODULE["git"]("diff", "--binary", "HEAD", root=self.source)

    def test_resume_serializes_cleanup_and_preserves_worktree(self):
        claim = self.claim()
        record = self.store.get(claim)

        class EmptyRegistry:
            @contextmanager
            def edit(self):
                yield {"leases": {}}

        def interleave(*_args):
            with self.assertRaisesRegex(Refused, "busy"):
                self.store.cleanup(claim, "fixture-owner", self.area / "resources")
            return SimpleNamespace(registry=EmptyRegistry())

        with patch.object(self.store, "resources", side_effect=interleave):
            result = self.store.resume(claim, "fixture-owner", self.area / "resources")
        self.assertEqual(result["phase"], "allocated")
        self.assertTrue(Path(record["worktree"]).is_dir())
        self.store.cleanup(claim, "fixture-owner", self.area / "resources")

    def test_resume_cannot_overwrite_a_changed_lifecycle_token(self):
        claim = self.claim()

        class EmptyRegistry:
            @contextmanager
            def edit(self):
                yield {"leases": {}}

        def replaced(*_args):
            with self.store.edit() as data:
                data["tasks"][claim]["lifecycle_token"] = "changed"
            return SimpleNamespace(registry=EmptyRegistry())

        with patch.object(self.store, "resources", side_effect=replaced):
            with self.assertRaisesRegex(Refused, "ownership changed"):
                self.store.resume(claim, "fixture-owner", self.area / "resources")
        self.assertEqual(self.store.get(claim)["phase"], "resuming")

    def test_closed_or_full_claim_refuses_before_creating_evidence_blobs(self):
        claim = self.claim()
        folder = self.store.root / "evidence" / claim
        before = set(folder.iterdir())
        self.store.cleanup(claim, "fixture-owner", self.area / "resources")
        with self.assertRaisesRegex(Refused, "Closed"):
            self.store.append(claim, "fixture-owner", "note", {"text": "must not write"})
        self.assertEqual(set(folder.iterdir()), before)
        claim = self.claim()
        folder = self.store.root / "evidence" / claim
        before = set(folder.iterdir())
        with self.store.edit() as data:
            data["tasks"][claim]["evidence"] *= 1000
        with self.assertRaisesRegex(Refused, "record bound"):
            self.store.append(claim, "fixture-owner", "note", {"text": "must not write"})
        self.assertEqual(set(folder.iterdir()), before)

    def test_orphaned_blob_storage_is_charged_before_new_writes(self):
        claim = self.claim()
        folder = self.store.root / "evidence" / claim
        with (folder / ("0" * 64 + ".json")).open("wb") as target:
            target.truncate(64 * 1024**2)
        before = set(folder.iterdir())
        with self.assertRaisesRegex(Refused, "storage bound"):
            self.store.append(claim, "fixture-owner", "note", {"text": "must not write"})
        self.assertEqual(set(folder.iterdir()), before)

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


class ProbeDispatchControls(unittest.TestCase):
    """HOST lifecycle/accounting controls, never native/provider qualification."""
    def fixture(self, area):
        claim = "m03-02-"+"a"*12
        row = {"packet": {**packet(task="M03.02"), "commands": ["rustfs-capabilities"],
                          "output_budget_bytes": 2*1024**2},
               "packet_sha256": "b"*64, "branch": "codex/task/"+claim,
               "worktree": str(area/"source"), "namespace": None,
               "phase": "allocated", "worker": None, "evidence": []}
        data = {"tasks": {claim: row}, "resource_root": None}
        store = object.__new__(TaskStore)
        store.root = area/"task"
        store.root.mkdir()
        @contextmanager
        def edit():
            changed = copy.deepcopy(data)
            yield changed
            data.clear()
            data.update(changed)
        store.edit = edit
        store.get = lambda _claim, _owner: copy.deepcopy(data["tasks"][claim])
        return store, data, claim

    def test_input_reservation_is_charged_before_publication_and_bad_sizes_refuse(self):
        with tempfile.TemporaryDirectory() as directory:
            store, data, claim = self.fixture(Path(directory))
            for invalid in (True, -1, 1024**2+1):
                with self.subTest(size=invalid), self.assertRaises(Refused):
                    store.append(claim, "fixture-owner", "input", {}, retained_input_bytes=invalid)
            store.append(claim, "fixture-owner", "input", {"sha256": "a"*64}, retained_input_bytes=1024)
            self.assertEqual(data["tasks"][claim]["probe_retained_input_bytes"], 1024)
            self.assertEqual(len(data["tasks"][claim]["evidence"]), 1)
            data["tasks"][claim]["probe_retained_input_bytes"] = 64*1024**2-1
            before = copy.deepcopy(data)
            with self.assertRaisesRegex(Refused, "Combined"):
                store.append(claim, "fixture-owner", "input", {})
            self.assertEqual(data, before)

    def test_physical_orphan_bytes_are_counted_and_redirected_slots_refuse(self):
        with tempfile.TemporaryDirectory() as directory:
            area = Path(directory)
            store, data, claim = self.fixture(area)
            registry = MODULE["SANDBOX"]["Registry"](area/"resources")
            namespace = "hsearch-m03-02-"+"b"*12
            row = data["tasks"][claim]
            row.update(namespace=namespace, probe_active=True)
            data["resource_root"] = str(registry.root)
            with registry.edit() as resources:
                resources["leases"][namespace] = {"task_claim": claim, "source": row["worktree"], "snapshot_schema": 2}
            slot = registry.root/"snapshots"/namespace/"rustfs-probe-inputs"/"rustfs-capabilities-before"
            slot.mkdir(parents=True)
            (slot/"orphan.json").write_bytes(b"x"*1500)
            self.assertEqual(store.retained_probe_inputs(row, registry.root), 1500)
            store.append(claim, "fixture-owner", "input", {}, retained_input_bytes=100)
            self.assertEqual(data["tasks"][claim]["probe_retained_input_bytes"], 1600)
            (slot/"nested").mkdir()
            with self.assertRaisesRegex(Refused, "Unknown retained"):
                store.append(claim, "fixture-owner", "input", {})

    def test_fixed_dispatch_rejects_wrong_task_small_packet_and_duplicate_attempt(self):
        with tempfile.TemporaryDirectory() as directory:
            store, data, claim = self.fixture(Path(directory))
            row = data["tasks"][claim]
            store.fingerprint = lambda _record: {"clean": True}
            for key, value in (("task_id", "M03.01"), ("output_budget_bytes", 16384)):
                old = row["packet"][key]
                row["packet"][key] = value
                with self.subTest(field=key), self.assertRaisesRegex(Refused, "outside"):
                    store.rustfs_capabilities(claim, "fixture-owner", Path(directory))
                row["packet"][key] = old
            row["probe_active"] = True
            with self.assertRaisesRegex(Refused, "already attempted"):
                store.rustfs_capabilities(claim, "fixture-owner", Path(directory))

    def test_known_failed_child_is_retained_before_controller_receives_it(self):
        with tempfile.TemporaryDirectory() as directory:
            area = Path(directory)
            store, data, claim = self.fixture(area)
            row = data["tasks"][claim]
            source = area/"source"
            source.mkdir()
            (source/"Cargo.lock").write_bytes(b"fixture")
            row["namespace"] = "hsearch-m03-02-"+"b"*12
            lease = {"task_claim": claim, "source": str(source), "resource_protocol": 2,
                     "snapshot_schema": 2, "image": "sha256:"+"c"*64, "phase": "ready", "config_sha256": "d"*64}
            resources = {"leases": {row["namespace"]: lease}}
            @contextmanager
            def registry_edit():
                yield resources
            registry = SimpleNamespace(get=lambda _namespace: lease, edit=registry_edit)
            failure = {"mode": "probe-checker-build", "exit_code": 1, "truncated": False, "output": "first failure"}
            def failed_run(*_args, **_kwargs):
                raise MODULE["SANDBOX"]["WorkloadFailed"](failure)
            sandbox = SimpleNamespace(registry=registry, run=failed_run,
                docker=SimpleNamespace(info={"Architecture": "x86_64"}),
                probe_storage=lambda _ns: ({"Image": "sha256:"+"e"*64, "Id": "f"*64},
                    {"config_sha256": "1"*64, "volume": "fixture-volume"}))
            store.resources = lambda *_args: sandbox
            store.fingerprint = lambda _record: {"clean": True, "source_tree_sha256": "2"*64}
            retained = []
            store.append = lambda _claim, _owner, kind, value, **_kw: retained.append((kind, value))
            def qualify(context):
                self.assertEqual(context.run("probe-checker-build"), failure)
                self.assertEqual(retained[0][0], "failed-command")
                import base64
                self.assertEqual(base64.b64decode(retained[0][1]["result"]["output_base64"]), b"first failure")
                raise Refused("stop on first build failure")
            controller = {"ProfileContext": runpy.run_path(str(Path(__file__).resolve().parents[1]/"rustfs_probe.py"))["ProfileContext"],
                          "qualify": qualify}
            with patch.dict(TaskStore.rustfs_capabilities.__globals__["runpy"].__dict__, run_path=lambda _path: controller):
                with self.assertRaisesRegex(Refused, "first build failure"):
                    store.rustfs_capabilities(claim, "fixture-owner", area/"resources")
            self.assertEqual(data["tasks"][claim]["phase"], "interrupted")
            self.assertIsNone(data["tasks"][claim]["worker"])
            self.assertTrue(data["tasks"][claim]["probe_active"])


class ProbeEvidenceFramingControls(unittest.TestCase):
    def test_worst_case_control_output_is_lossless_and_fits_original_packet(self):
        import base64
        raw = "\x00"*(1024**2)
        result = {"output": raw, "mode": "rustfs-capabilities-before", "exit_code": 1,
                  "truncated": False, "encoding": "utf-8-replacement"}
        self.assertGreater(len(MODULE["encoded"](result)), 2*1024**2)
        stored = MODULE["probe_evidence_result"](result, 1024**2)
        self.assertEqual(base64.b64decode(stored["output_base64"]), raw.encode())
        self.assertEqual(stored["output_sha256"], hashlib.sha256(raw.encode()).hexdigest())
        self.assertLess(len(MODULE["encoded"]({"source": {"digest": "a"*64}, "result": stored})), 2*1024**2)
        self.assertEqual(result["output"], raw)
        with self.assertRaisesRegex(Refused, "admitted output"):
            MODULE["probe_evidence_result"](result, 1024**2-1)


class RawProbeOutputControls(unittest.TestCase):
    def result(self, stdout, stderr):
        return {"output": stdout.decode("utf-8", errors="replace"),
                "raw_streams": {"stdout": stdout, "stderr": stderr},
                "output_protocol": "rustfs-raw-streams-v1", "exit_code": 1, "truncated": False}

    def test_exact_both_pipes_control_and_non_utf8_bytes_share_one_packet_bound(self):
        streams = {"stdout": b"\x00\xff"*(256*1024), "stderr": b"\x80\n"*(256*1024)}
        result = self.result(**streams)
        stored = MODULE["probe_evidence_result"](result, 1024**2)
        self.assertEqual(MODULE["decode_probe_output"](stored), streams)
        self.assertLess(len(MODULE["encoded"]({"result": stored, "source": {"digest": "a"*64}})), 2*1024**2)
        self.assertEqual(result["raw_streams"], streams)
        with self.assertRaisesRegex(Refused, "admitted output"):
            MODULE["probe_evidence_result"](result, 1024**2-1)
        corrupted = copy.deepcopy(stored)
        corrupted["output_streams"]["stderr"]["sha256"] = "0"*64
        with self.assertRaisesRegex(Refused, "binding changed"):
            MODULE["decode_probe_output"](corrupted)

    def test_raw_capture_preserves_both_os_pipes_and_refuses_aggregate_overflow(self):
        code = "import os;os.write(1,b'out\\x00\\xff');os.write(2,b'err\\x80\\n')"
        capture = MODULE["SANDBOX"]["bounded_capture"]
        streams = capture([sys.executable, "-B", "-c", code], timeout=10, limit=10, raw_streams=True)
        self.assertEqual(streams, {"stdout": b"out\x00\xff", "stderr": b"err\x80\n"})
        with self.assertRaisesRegex(Refused, "output exceeds bound"):
            capture([sys.executable, "-B", "-c", code], timeout=10, limit=9, raw_streams=True)

    def test_encoded_secret_cannot_bypass_decoded_nested_public_export_checks(self):
        for secret in (b"Authorization: bearer PRIVATE", b"X-Amz-Credential=PRIVATE", b"actual-fixture-secret"):
            stored = MODULE["probe_evidence_result"](self.result(b"safe", b"\xff"+secret+b"\x00"), 1024)
            self.assertNotIn(secret.decode(), MODULE["encoded"](stored).decode())
            with self.assertRaisesRegex(Refused, "Secret-bearing"):
                MODULE["require_public_probe_output"]({"nested": [stored]}, forbidden=(b"actual-fixture-secret",))
        MODULE["require_public_probe_output"](MODULE["probe_evidence_result"](self.result(b"safe\x00", b"\xff"), 1024))

    def test_legacy_failed_packet_remains_immutable_and_truthfully_decoded_utf8(self):
        legacy = MODULE["probe_evidence_result"]({"output": "legacy\x00UTF8", "encoding": "utf-8-replacement"}, 1024)
        packet = {"source": {"head": "a"*40}, "result": legacy}
        frozen = MODULE["encoded"](packet)
        self.assertEqual(legacy["output_representation"], "base64-of-utf8-output")
        self.assertEqual(MODULE["decode_probe_output"](legacy), {"stdout": b"legacy\x00UTF8", "stderr": b""})
        self.assertEqual(MODULE["encoded"](packet), frozen)
        bad = copy.deepcopy(legacy)
        bad["output_sha256"] = "0"*64
        with self.assertRaisesRegex(Refused, "binding changed"):
            MODULE["decode_probe_output"](bad)


class ProbePublicExportControls(unittest.TestCase):
    def test_last_encoded_secret_refuses_before_first_public_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            private = root/"private"
            output = root/"public"
            output.mkdir()
            claim = "m03-02-"+"a"*12
            folder = private/"evidence"/claim
            folder.mkdir(parents=True)
            safe = MODULE["probe_evidence_result"]({"output": "safe"}, 1024)
            secret = MODULE["probe_evidence_result"]({"output": "Authorization: PRIVATE-FIXTURE"}, 1024)
            records = []
            for value in (safe, secret):
                raw = MODULE["encoded"]({"result": value})
                digest = hashlib.sha256(raw).hexdigest()
                (folder/(digest+".json")).write_bytes(raw)
                records.append({"sha256": digest, "bytes": len(raw)})
            store = SimpleNamespace(root=private, evidence=lambda *_args: records)
            with self.assertRaisesRegex(Refused, "Secret-bearing"):
                MODULE["export_probe_evidence"](store, claim, "fixture", output)
            self.assertEqual(list(output.iterdir()), [])
            records.pop()
            self.assertEqual(MODULE["export_probe_evidence"](store, claim, "fixture", output), records)
            self.assertEqual((output/"evidence"/(records[0]["sha256"]+".json")).read_bytes(), (folder/(records[0]["sha256"]+".json")).read_bytes())


class CampaignDispatchControls(unittest.TestCase):
    """Synthetic lifecycle dispatch only; no TaskStore claim or real resource."""
    def fixture(self, root, fail=False):
        claim='m02-10-'+'a'*12;events=[];source={'clean':True,'head':'b'*40,'digest':'c'*64}
        row={'packet':{**packet(task='M02.10'),'research_campaign':'durability-v1',
                       'commands':['prefetch','check'],'output_budget_bytes':1024**2},
             'packet_sha256':'d'*64,'phase':'allocated','worker':None,'namespace':None,
             'worktree':str(root/'source'),'evidence':[]}
        data={'tasks':{claim:row}}
        store=object.__new__(TaskStore);store.root=root/'state'
        @contextmanager
        def edit():yield data
        store.edit=edit;store.get=lambda *a:copy.deepcopy(data['tasks'][claim])
        store.fingerprint=lambda row:dict(source)
        store.append=lambda claim,owner,kind,value:events.append(('append',kind,value))
        class Journal:
            def __init__(self,*args):self.output=root/'public';self.deadline=SimpleNamespace(require=lambda seconds:None)
            def admit_task_reserve(self):events.append(('reserve',))
            def terminal(self,*args):events.append(('checkpoint',));return {'event':'fixture-reference'}
            def verify(self,*args):events.append(('verify-checkpoint',))
        def run(namespace,mode,**options):
            self.assertEqual(options['output_budget'],512*1024)
            events.append(('worker',mode))
            ref=options['checkpoint']({}, {})
            result={'mode':mode,'exit_code':7 if fail else 0,'checkpoint':ref,'truncated':False,'output':'x'*600000}
            if fail:raise MODULE['SANDBOX']['WorkloadFailed'](result)
            return result
        sandbox=SimpleNamespace(docker=SimpleNamespace(),up=lambda *a,**kw:'hsearch-m02-10-'+'e'*12,run=run,
                                down=lambda namespace:events.append(('down',namespace)))
        store.resources=lambda *args:sandbox
        def campaign(*args,**options):
            events.append(('campaign',));self.assertTrue(callable(options['checkpoint']))
            args[3](None)
            return {'sample_count':12,'scope':'Synthetic dispatch fixture; no native evidence'}
        def load(path):
            return {'Journal':Journal} if str(path).endswith('campaign_checkpoint.py') else {'run_campaign':campaign}
        return store,data,claim,events,source,load

    def test_orchestration_budget_is_charged_in_existing_claim_and_never_resets(self):
        with tempfile.TemporaryDirectory() as temporary:
            store,data,claim,events,source,load=self.fixture(Path(temporary).resolve())
            row=data['tasks'][claim];owner=row['packet']['owner_session'];row['campaign_docker_calls']=1023
            charge=store.campaign_docker_charge(claim,owner)
            charge();self.assertEqual(row['campaign_docker_calls'],1024)
            with self.assertRaisesRegex(Refused,'budget exhausted'):charge()
            self.assertEqual(row['campaign_docker_calls'],1024)
            with self.assertRaisesRegex(Refused,'owner/lifecycle'):store.campaign_docker_charge(claim,'foreign')()
            self.assertEqual(row['campaign_docker_calls'],1024)

    def test_prefetch_then_check_checkpoint_before_down_and_compact_final_references(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve();store,data,claim,events,source,load=self.fixture(root)
            with patch.object(MODULE['runpy'],'run_path',side_effect=load):
                store.run(claim,'owner','prefetch',root/'resources',public_output=root/'public',deadline=10000)
                result=store.run(claim,'owner','check',root/'resources',public_output=root/'public',deadline=10000)
            self.assertEqual(data['tasks'][claim]['campaign_started_modes'],['prefetch','check'])
            self.assertEqual(result['campaign']['sample_count'],12)
            self.assertLess(events.index(('verify-checkpoint',)),next(i for i,e in enumerate(events) if e[0]=='down'))
            self.assertEqual(data['tasks'][claim]['phase'],'allocated')
            records=[e[2] for e in events if e[0]=='append']
            self.assertTrue(all(len(MODULE['encoded'](value))<16384 for value in records))
            self.assertTrue(all('output' not in value['result'] for value in records))

    def test_missing_prefetch_duplicate_attempt_and_changed_binding_refuse_without_worker(self):
        for failure in ('missing-prefetch','duplicate','changed-source'):
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as temporary:
                root=Path(temporary).resolve();store,data,claim,events,source,load=self.fixture(root)
                with patch.object(MODULE['runpy'],'run_path',side_effect=load):
                    if failure!='missing-prefetch':store.run(claim,'owner','prefetch',root/'resources',public_output=root/'public',deadline=10000)
                    before=sum(e[0]=='worker' for e in events)
                    if failure=='changed-source':source['digest']='f'*64
                    with self.assertRaises(Refused):
                        store.run(claim,'owner','prefetch' if failure=='duplicate' else 'check',root/'resources',public_output=root/'public',deadline=10000)
                    self.assertEqual(sum(e[0]=='worker' for e in events),before)

    def test_failed_command_keeps_interrupted_stage_and_compact_record_without_retry(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary).resolve();store,data,claim,events,source,load=self.fixture(root,fail=True)
            with patch.object(MODULE['runpy'],'run_path',side_effect=load):
                with self.assertRaises(MODULE['SANDBOX']['WorkloadFailed']):
                    store.run(claim,'owner','prefetch',root/'resources',public_output=root/'public',deadline=10000)
            self.assertEqual(data['tasks'][claim]['phase'],'interrupted')
            self.assertEqual(data['tasks'][claim]['campaign_started_modes'],['prefetch'])
            failure=[e[2] for e in events if e[:2]==('append','failed-command')][0]
            self.assertLess(len(MODULE['encoded'](failure)),16384)
            self.assertNotIn('output',failure['result'])
            self.assertFalse(any(e[0]=='down' for e in events))
