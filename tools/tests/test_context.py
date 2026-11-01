"""Independent bounded retrieval, source freshness and accounting controls."""
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest

API = runpy.run_path(str(Path(__file__).resolve().parents[1] / "context.py"))
Refused = API["Refused"]


def req(operation, **fields):
    return {"schema_version": 1, "operation": operation, **fields}


class RequestControls(unittest.TestCase):
    def test_exact_schema_and_operation_fields_reject_unsafe_requests(self):
        for value in (req("shell"), req("outline", file="../private.rs"), req("outline", file=".env"),
                      req("outline", file="a.rs", apply=True), req("search", pattern="x", max_items=True),
                      req("reference", file="a.rs", line=1, find="none"),
                      req("rename-preview", file="a.rs", line=1, find="<|>thing", new_name=True)):
            with self.subTest(value=value), self.assertRaises(Refused):
                API["request"](value)


@unittest.skipUnless(os.name == "posix" and shutil.which("git"), "Real Git context fixtures run in the Linux image")
class RetrievalControls(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.area = Path(self.temporary.name)
        self.source = self.area / "source"
        (self.source / "docs/plan").mkdir(parents=True)
        (self.source / "src").mkdir()
        self.text = '// Greek: λ and snow: 雪\npub struct User;\nfn greet() {}\nfn main() { greet(); }\n'
        (self.source / "src/main.rs").write_text(self.text, encoding="utf-8")
        (self.source / "src/empty.rs").write_text("")
        (self.source / "docs/plan/tasks.json").write_text(json.dumps({"milestones": [{"id": "M01", "status": "in_progress", "tasks": [
            {"id": "M01.04", "status": "planned", "depends_on": []}]}]}))
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Context fixture")
        self.git("config", "user.email", "context@example.invalid")
        self.git("config", "core.autocrlf", "false")
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")
        base = self.git("rev-parse", "HEAD").strip()
        self.store = API["TASK"]["TaskStore"](self.area / "state", self.source)
        self.owner = "context-fixture"
        packet = {"schema_version": 1, "task_id": "M01.04", "base_revision": base, "owner_session": self.owner,
                  "goal": "Exercise bounded context", "owned_paths": ["src"], "assumptions": [],
                  "acceptance": ["Fresh source and exact bytes"], "negative_cases": ["Stale results refuse"],
                  "commands": ["check"], "output_budget_bytes": 65536, "expected_evidence": ["Content-bound response"]}
        self.claim = self.store.claim(packet)
        self.root = Path(self.store.get(self.claim)["worktree"])

    def tearDown(self):
        self.temporary.cleanup()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.source, stderr=subprocess.PIPE).decode()

    def route(self, operation, *, semantic=None, **fields):
        return API["route"](self.store, self.claim, self.owner, req(operation, **fields), self.area / "resources", semantic=semantic)

    def test_utf8_source_coordinates_and_payload_accounting_are_exact(self):
        result = self.route("slice", file="src/main.rs", start_line=1, end_line=2)
        self.assertEqual(result["data"]["text"], "".join(self.text.splitlines(keepends=True)[:2]))
        self.assertEqual(len(API["encoded"](result)), result["response_payload_bytes"])
        self.assertEqual(result["estimated_tokens_bytes_div4"], (result["response_payload_bytes"] + 3) // 4)
        self.assertTrue(result["token_count_is_estimate"])
        self.assertEqual(len(result["evidence_sha256"]), 64)
        self.store.evidence(self.claim, self.owner)

    def test_default_cli_preserves_legacy_metadata_shape_without_accounting_event(self):
        value = json.loads(subprocess.check_output([sys.executable, str(API["ROOT"] / "tools/context.py"),
            "context", self.claim, "--owner", self.owner, "--state-root", str(self.area / "state")], cwd=self.source))
        self.assertEqual(value["claim"], self.claim)
        self.assertEqual(value["packet"]["owner_session"], self.owner)
        self.assertIn("context_bytes", value)
        self.assertNotIn("operation", value)
        self.assertEqual(API["prior_context"](self.store, self.claim, self.owner), [])

    def test_full_file_requires_reason_and_recorded_narrower_attempt_at_same_source(self):
        with self.assertRaisesRegex(Refused, "requires a concrete reason"):
            self.route("full", file="src/main.rs")
        with self.assertRaisesRegex(Refused, "narrower attempt"):
            self.route("full", file="src/main.rs", reason="Need the complete small unit to compare both callers")
        self.route("slice", file="src/main.rs", start_line=2, end_line=3)
        result = self.route("full", file="src/main.rs", reason="Need complete caller context after reading the declaration slice")
        self.assertEqual(result["data"]["text"], self.text)
        self.assertTrue(result["data"]["complete_file"])
        (self.root / "src/main.rs").write_text(self.text + "// new source\n", encoding="utf-8")
        with self.assertRaisesRegex(Refused, "narrower attempt"):
            self.route("full", file="src/main.rs", reason="The prior attempt belongs to an older source")

    def test_search_pages_bind_source_without_duplicates(self):
        first = self.route("search", pattern="greet", path="src", max_items=1)
        self.assertTrue(first["data"]["has_more"])
        with self.assertRaisesRegex(Refused, "pagination requires"):
            self.route("search", pattern="greet", cursor=1)
        second = self.route("search", pattern="greet", path="src", cursor=1, max_items=1, expected_source=first["source"]["digest"])
        self.assertEqual([first["data"]["matches"][0]["line"], second["data"]["matches"][0]["line"]], [3, 4])
        self.assertFalse(second["data"]["has_more"])
        (self.root / "src/main.rs").write_text(self.text + "// edit\n", encoding="utf-8")
        with self.assertRaisesRegex(Refused, "stale source"):
            self.route("search", pattern="greet", cursor=1, expected_source=first["source"]["digest"])

    def test_over_budget_response_is_not_published_as_success(self):
        (self.root / "src/main.rs").write_text("// " + "x" * 4000 + "\n")
        before = len(self.store.evidence(self.claim, self.owner))
        with self.assertRaisesRegex(Refused, "exceeds byte budget"):
            self.route("slice", file="src/main.rs", max_bytes=1024)
        self.assertEqual(len(self.store.evidence(self.claim, self.owner)), before)

    def test_stale_semantic_response_is_refused_and_source_is_preserved(self):
        def changes_source(*_args):
            (self.root / "src/main.rs").write_text("// concurrent edit\n")
            return {"status": "semantic", "data": {"items": []}}
        with self.assertRaisesRegex(Refused, "changed during retrieval"):
            self.route("outline", file="src/main.rs", semantic=changes_source)
        self.assertEqual((self.root / "src/main.rs").read_text(), "// concurrent edit\n")

    def test_missing_semantics_fallback_is_never_a_rename_plan_or_compile_pass(self):
        def missing(_store, _claim, _owner, value, _resources, _source):
            return {"status": "unavailable", "semantic": False, "reason": "Pinned LSP unavailable"}
        result = self.route("rename-preview", file="src/main.rs", line=3, find="fn <|>greet", new_name="hello", semantic=missing)
        self.assertEqual(result["status"], "fallback")
        self.assertFalse(result["semantic"])
        self.assertTrue(result["data"]["not_a_semantic_result_or_rename_plan"])
        diagnostic = self.route("diagnostics", file="src/main.rs", semantic=missing)
        self.assertTrue(diagnostic["cargo_check_required"])
        self.assertTrue(diagnostic["data"]["authoritative_cargo_check_required"])
        with self.assertRaisesRegex(Refused, "cannot advertise applied"):
            self.route("rename-preview", file="src/main.rs", line=3, find="fn <|>greet", new_name="hello",
                       semantic=lambda *_args: {"status": "semantic", "data": {"applied": True}})
        self.assertEqual((self.root / "src/main.rs").read_text(encoding="utf-8"), self.text)

    def test_workflow_accounting_includes_prior_requests_and_responses(self):
        one = self.route("search", pattern="User")
        two = self.route("slice", file="src/main.rs", start_line=1, end_line=2)
        result = self.route("stats")["data"]
        self.assertEqual(result["previous_context_events"], 2)
        self.assertEqual(result["response_payload_bytes"], one["response_payload_bytes"] + two["response_payload_bytes"])
        requests = sum(len(API["encoded"](value)) for value in (req("search", pattern="User"), req("slice", file="src/main.rs", start_line=1, end_line=2)))
        self.assertEqual(result["canonical_request_bytes"], requests)
        self.assertTrue(result["token_counts_are_estimates"])
        self.assertIsNone(result["tokenizer"])

    def test_aliases_and_oversized_source_units_refuse(self):
        outside = self.area / "outside.rs"
        outside.write_text("// private fixture\n")
        (self.root / "src/alias.rs").symlink_to(outside)
        with self.assertRaisesRegex(Refused, "redirected"):
            self.route("slice", file="src/alias.rs")
        (self.root / "src/alias.rs").unlink()
        (self.root / "src/main.rs").write_bytes(b"x" * 65537)
        with self.assertRaisesRegex(Refused, "retrieval limit"):
            self.route("slice", file="src/main.rs")


if __name__ == "__main__":
    unittest.main()
