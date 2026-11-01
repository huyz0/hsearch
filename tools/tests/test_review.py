"""Independent protocol counterexamples; this does not simulate AI defect detection."""
import copy
import io
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

API = runpy.run_path(str(Path(__file__).resolve().parents[1] / "review.py"))
Refused = API["Refused"]


class SchemaControls(unittest.TestCase):
    def test_risk_routing_is_derived_from_actual_paths_and_unknown_code_is_sensitive(self):
        with patch.dict(API["routes"].__globals__, NARRATIVE_DOCS=frozenset({"docs/known-narrative.md"})):
            self.assertEqual(API["routes"](["docs/known-narrative.md"]), ["correctness_tests_claims"])
            self.assertIn("architecture_security", API["routes"](["docs/known-narrative.md", "docs/unknown.md"]))
        for path in ("tools/review.py", "crates/types/src/lib.rs", "docs/plan/security.md", ".github/workflows/native.yml", "new_unknown_file",
                     "docs/plan/tools/plan.py", "docs/plan/tasks.json", "docs/generated.rs", "docs/plan/review-gates.md", "docs/plan/sandbox.md", "docs/sandbox.md",
                     "docs/usage.md", "docs/authentication.md", "docs/access-control.md", "docs/contracts/durability.md", "docs/operating-profiles.md", "docs/new_contract.txt"):
            self.assertIn("architecture_security", API["routes"]([path]))


class InputControls(unittest.TestCase):
    def test_regular_inputs_are_bounded_even_when_the_size_snapshot_is_stale(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "input.json"
            path.write_bytes(b'{"value":1}')
            self.assertEqual(API["bounded_json"](path, 16), {"value": 1})
            path.write_bytes(b'{"value":"' + b"x" * 100 + b'"}')
            with patch.object(API["os"], "fstat", return_value=SimpleNamespace(st_mode=0o100644, st_size=0)):
                with self.assertRaisesRegex(Refused, "grew beyond"):
                    API["bounded_json"](path, 16)

    def test_over_nested_json_is_refused_without_unbounded_schema_recursion(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "input.json"
            path.write_bytes(b"[" * 2000 + b"]" * 2000)
            with self.assertRaisesRegex(Refused, "over-nested"):
                API["bounded_json"](path, 8192)
            value = {"quoted_source": '\\"[[{{' * 100}
            path.write_text(json.dumps(value), encoding="utf-8")
            self.assertEqual(API["bounded_json"](path, 8192), value)

    @unittest.skipUnless(os.name == "posix", "FIFO and no-follow controls require native POSIX")
    def test_fifo_and_redirected_input_are_refused_before_reading(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "input.json"
            os.mkfifo(path)
            with self.assertRaisesRegex(Refused, "regular file"):
                API["read_json"](path)
            path.unlink()
            target = Path(folder) / "target.json"
            target.write_text("{}", encoding="utf-8")
            path.symlink_to(target)
            with self.assertRaisesRegex(Refused, "redirected"):
                API["read_json"](path)


@unittest.skipUnless(shutil.which("git"), "Protocol source-binding controls need real Git")
class ProtocolControls(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.area = Path(self.temporary.name)
        self.root = self.area / "source"
        (self.root / "docs/plan").mkdir(parents=True)
        (self.root / "src").mkdir()
        (self.root / "docs/plan/tasks.json").write_text(json.dumps({"milestones": [{"id": "M01", "tasks": [{"id": "M01.05", "status": "planned", "depends_on": []}]}]}), encoding="utf-8")
        (self.root / "src/lib.rs").write_text("pub fn add_one(x: i32) -> i32 { x + 1 }\npub fn authenticate(expected: &str, supplied: &str) -> bool { expected == supplied }\n#[test] fn arithmetic_oracle() { assert_eq!(add_one(0), 1); }\n", encoding="utf-8", newline="\n")
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Review fixture")
        self.git("config", "user.email", "review@example.invalid")
        self.git("config", "core.autocrlf", "false")
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD")
        (self.root / "src/lib.rs").write_text("pub fn add_one(x: i32) -> i32 { x + 2 }\npub fn authenticate(_expected: &str, _supplied: &str) -> bool { true }\n#[test] fn arithmetic_oracle() { assert_eq!(add_one(0), add_one(0)); }\n", encoding="utf-8", newline="\n")
        self.git("commit", "-qam", "seed concrete arithmetic regression")
        self.store = API["ReviewStore"](self.area / "state", self.root)
        self.reviewers = {"correctness_tests_claims": "separate-session-correctness", "architecture_security": "separate-session-security"}
        self.checks = [{"name": "deterministic-full", "head": self.git("rev-parse", "HEAD"), "conclusion": "success", "evidence": "Fixture observed checker input; not an actual production check claim"}]
        self.checks.append({**self.checks[0], "name": "ci-ok"})

    def tearDown(self):
        self.temporary.cleanup()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, stderr=subprocess.PIPE, timeout=20).decode().strip()

    def begin(self, **fields):
        return self.store.begin("M01.05", self.base, fields.get("implementer", "implementer"),
                                fields.get("reviewers", self.reviewers), fields.get("checks", self.checks), "Review concrete changed code and previous findings")

    def verdict(self, packet, role, findings=None):
        findings = findings or []
        value = packet["packet"]
        return {"schema_version": 1, "packet_sha256": packet["packet_sha256"],
                **{key: value[key] for key in ("head", "tree", "source_digest")}, "role": role,
                "reviewer_session": self.reviewers[role], "conclusion": "blocked" if any(item["priority"] != "P3" for item in findings) else "clear",
                "basis": "Independent fixture source and concrete expected output", "findings": findings,
                "resolutions": [{"review_key": item["review_key"], "evidence": "Independent source delta and regression control verified"}
                                for item in value["previous_findings"] if item["priority"] != "P3"]}

    def finding(self, category="correctness"):
        return {"id": category, "priority": "P1", "file": "src/lib.rs", "line": {"correctness": 1, "security": 2, "weak_oracle": 3}[category],
                "reproduction": {"correctness": "add_one(0) returns 2", "security": "authenticate(secret, forged) returns true", "weak_oracle": "assert_eq!(add_one(0), add_one(0)) passes for the seeded wrong result"}[category],
                "expected": "Independent reference behavior must match", "observed": "Seeded fixture contradicts the independent oracle"}

    def test_empty_independent_reviews_pass_only_when_all_roles_and_checks_present(self):
        packet = self.begin()
        with self.assertRaisesRegex(Refused, "missing/pending"):
            self.store.gate("M01.05", self.checks)
        self.store.record(self.verdict(packet, "correctness_tests_claims"))
        with self.assertRaisesRegex(Refused, "missing/pending"):
            self.store.gate("M01.05", self.checks)
        self.store.record(self.verdict(packet, "architecture_security"))
        proof = self.store.gate("M01.05", self.checks)
        self.assertEqual(proof["gate"], "pass")
        self.assertFalse(proof["formal_github_review"])
        self.assertEqual(proof["checks"], self.checks)
        self.assertEqual(proof["checks_sha256"], API["hashlib"].sha256(API["encoded"](self.checks)).hexdigest())

    def test_status_recovers_issued_packet_and_utf8_output_without_charging_another_round(self):
        packet = self.store.begin("M01.05", self.base, "implementer", self.reviewers, self.checks, "Inspect quoted source: 雪 λ")
        self.store.record(self.verdict(packet, "correctness_tests_claims"))
        (self.root / "src/lib.rs").write_text("// changed after packet issuance\n", encoding="utf-8", newline="\n")
        buffer = io.BytesIO()
        console = io.TextIOWrapper(buffer, encoding="ascii")
        with patch.dict(API["main"].__globals__, ReviewStore=lambda _state: self.store), \
             patch.object(sys, "argv", ["review.py", "status", "M01.05"]), patch.object(sys, "stdout", console):
            self.assertEqual(API["main"](), 0)
        value = json.loads(buffer.getvalue().decode("utf-8"))
        self.assertEqual(value["packet_sha256"], packet["packet_sha256"])
        self.assertEqual(value["rounds_issued"], 1)
        self.assertEqual(set(value["recorded_verdicts"]), {"correctness_tests_claims"})
        self.assertTrue(value["not_a_current_validation_verdict"])

    def test_seeded_correctness_security_and_weak_oracle_findings_block(self):
        for category in ("correctness", "security", "weak_oracle"):
            packet = self.begin()
            self.store.record(self.verdict(packet, "correctness_tests_claims", [self.finding(category)]))
            self.store.record(self.verdict(packet, "architecture_security"))
            with self.assertRaisesRegex(Refused, "Blocking findings"):
                self.store.gate("M01.05", self.checks)

    def test_same_session_missing_risk_role_and_stale_checks_refuse_before_round_is_charged(self):
        for fields in ({"implementer": self.reviewers["correctness_tests_claims"]},
                       {"reviewers": {"correctness_tests_claims": "one-session"}},
                       {"checks": [{**self.checks[0], "head": self.base}]}, {"checks": self.checks * 2}):
            with self.subTest(fields=fields), self.assertRaises(Refused):
                self.begin(**fields)
        self.assertEqual(self.begin()["packet"]["round"], 1)

    def test_wrong_reviewer_forged_tree_and_inconsistent_conclusion_refuse(self):
        packet = self.begin()
        for change in ({"reviewer_session": "implementer"}, {"tree": "0" * 40},
                       {"findings": [self.finding()], "conclusion": "clear"}, {"extra": "untrusted"}):
            with self.subTest(change=change), self.assertRaises(Refused):
                self.store.record({**self.verdict(packet, "correctness_tests_claims"), **change})

    def test_reviewed_code_change_and_hidden_dirty_bytes_invalidate_verdict(self):
        packet = self.begin()
        self.store.record(self.verdict(packet, "correctness_tests_claims"))
        self.store.record(self.verdict(packet, "architecture_security"))
        (self.root / "src/lib.rs").write_text("// actual changed compiler input\n", encoding="utf-8", newline="\n")
        self.git("update-index", "--assume-unchanged", "src/lib.rs")
        with self.assertRaisesRegex(Refused, "Hidden index"):
            self.store.gate("M01.05", self.checks)
        self.git("update-index", "--no-assume-unchanged", "src/lib.rs")
        self.git("commit", "-qam", "new tree")
        with self.assertRaisesRegex(Refused, "stale"):
            self.store.gate("M01.05", self.checks)

    def test_durable_counter_survives_new_store_and_changed_diff_through_ceiling(self):
        for number in range(1, 4):
            self.assertEqual(self.begin()["packet"]["round"], number)
            self.store = API["ReviewStore"](self.area / "state", self.root)
            (self.root / "src/lib.rs").write_text(f"// changed diff {number}\n", encoding="utf-8", newline="\n")
            self.git("commit", "-qam", f"iteration {number}")
            for check in self.checks:
                check["head"] = self.git("rev-parse", "HEAD")
        with self.assertRaisesRegex(Refused, "split/redesign"):
            self.begin()

    def test_follow_up_cannot_omit_a_previously_required_check(self):
        self.begin()
        (self.root / "docs/notes.md").write_text("follow-up\n", encoding="utf-8", newline="\n")
        self.git("add", ".")
        self.git("commit", "-qm", "follow-up")
        head = self.git("rev-parse", "HEAD")
        cheap = [{**self.checks[0], "name": "cheap-only", "head": head}]
        packet = self.begin(checks=cheap)
        self.assertEqual(set(packet["packet"]["required_check_names"]), {"deterministic-full", "ci-ok", "cheap-only"})
        for role in self.reviewers:
            self.store.record(self.verdict(packet, role))
        with self.assertRaisesRegex(Refused, "check evidence"):
            self.store.gate("M01.05", cheap + [{**cheap[0], "name": "ci-ok"}])
        self.store.gate("M01.05", cheap + [{**check, "head": head} for check in self.checks])

    def test_follow_up_base_is_immutable_and_risk_roles_survive_a_source_revert(self):
        self.begin()
        self.git("checkout", self.base, "--", "src/lib.rs")
        self.git("commit", "-qm", "repair by reverting seeded code")
        repair = self.git("rev-parse", "HEAD")
        (self.root / "docs/notes.md").write_text("narrative only\n", encoding="utf-8", newline="\n")
        self.git("add", ".")
        self.git("commit", "-qm", "follow-up narrative")
        for check in self.checks:
            check["head"] = self.git("rev-parse", "HEAD")
        with self.assertRaisesRegex(Refused, "original task base"):
            self.store.begin("M01.05", repair, "implementer", {"correctness_tests_claims": self.reviewers["correctness_tests_claims"]}, self.checks, "Attempted narrower base")
        # Positively approve only this fixture's narrative to exercise the
        # cumulative floor even when current paths would otherwise be lighter.
        with patch.dict(API["routes"].__globals__, NARRATIVE_DOCS=frozenset({"docs/notes.md"})):
            with self.assertRaisesRegex(Refused, "Required reviewers"):
                self.begin(reviewers={"correctness_tests_claims": self.reviewers["correctness_tests_claims"]})
            self.assertEqual(set(self.begin()["packet"]["required_roles"]), set(self.reviewers))

    def test_new_behavioral_markdown_cannot_begin_or_gate_with_only_one_review(self):
        path = self.root / "docs/authentication.md"
        path.write_text("Require the exact configured token.\n", encoding="utf-8", newline="\n")
        self.git("add", ".")
        self.git("commit", "-qm", "baseline authentication contract")
        base = self.git("rev-parse", "HEAD")
        path.write_text("Accept any bearer token.\n", encoding="utf-8", newline="\n")
        self.git("commit", "-qam", "seed behavioral documentation downgrade")
        for check in self.checks:
            check["head"] = self.git("rev-parse", "HEAD")
        with self.assertRaisesRegex(Refused, "Required reviewers"):
            self.store.begin("M01.05", base, "implementer", {"correctness_tests_claims": self.reviewers["correctness_tests_claims"]}, self.checks, "Only a document changed")
        packet = self.store.begin("M01.05", base, "implementer", self.reviewers, self.checks, "Review the actual behavioral contract")
        self.assertEqual(packet["packet"]["round"], 1)
        self.store.record(self.verdict(packet, "correctness_tests_claims"))
        with self.assertRaisesRegex(Refused, "missing/pending"):
            self.store.gate("M01.05", self.checks)

    def test_previous_implementer_cannot_become_an_independent_follow_up_reviewer(self):
        self.begin()
        with self.assertRaisesRegex(Refused, "every task implementer"):
            self.begin(implementer="replacement-implementer", reviewers={**self.reviewers, "architecture_security": "implementer"})

    def test_check_cardinality_is_coherent_before_charging_a_round(self):
        checks = [{**self.checks[0], "name": f"local-{number}"} for number in range(32)]
        with self.assertRaisesRegex(Refused, "check-name bound"):
            self.begin(checks=checks)
        checks = checks[:31]
        packet = self.begin(checks=checks)
        self.assertEqual(packet["packet"]["round"], 1)
        for role in self.reviewers:
            self.store.record(self.verdict(packet, role))
        self.assertEqual(self.store.gate("M01.05", checks + [{**self.checks[0], "name": "ci-ok"}])["gate"], "pass")

    def test_prior_blocker_requires_explicit_resolution_and_reviews_cannot_be_overwritten(self):
        first = self.begin()
        self.store.record(self.verdict(first, "correctness_tests_claims", [self.finding()]))
        with self.assertRaisesRegex(Refused, "immutable"):
            self.store.record(self.verdict(first, "correctness_tests_claims"))
        second = self.begin()
        value = self.verdict(second, "correctness_tests_claims")
        with self.assertRaisesRegex(Refused, "prior blocking"):
            self.store.record({**value, "resolutions": []})
        self.store.record(value)

    def test_stale_check_snapshots_and_corrupted_verdict_content_do_not_pass(self):
        packet = self.begin()
        for role in self.reviewers:
            self.store.record(self.verdict(packet, role))
        with self.assertRaisesRegex(Refused, "check evidence"):
            self.store.gate("M01.05", [{**self.checks[0], "head": self.base}])
        data = json.loads(self.store.path.read_text(encoding="utf-8"))
        data["tasks"]["M01.05"][-1]["verdicts"]["architecture_security"]["basis"] = "corrupted"
        self.store.path.write_text(json.dumps(data), encoding="utf-8")
        with self.assertRaisesRegex(Refused, "content changed"):
            self.store.gate("M01.05", self.checks)


if __name__ == "__main__":
    unittest.main()
