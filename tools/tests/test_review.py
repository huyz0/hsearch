"""Independent protocol counterexamples; this does not simulate AI defect detection."""
import copy
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import tempfile
import unittest

API = runpy.run_path(str(Path(__file__).resolve().parents[1] / "review.py"))
Refused = API["Refused"]


class SchemaControls(unittest.TestCase):
    def test_risk_routing_is_derived_from_actual_paths_and_unknown_code_is_sensitive(self):
        self.assertEqual(API["routes"](["docs/usage.md"]), ["correctness_tests_claims"])
        for path in ("tools/review.py", "crates/types/src/lib.rs", "docs/plan/security.md", ".github/workflows/native.yml", "new_unknown_file"):
            self.assertIn("architecture_security", API["routes"]([path]))


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
