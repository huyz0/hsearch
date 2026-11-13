"""Red controls for false API claims, unbounded design and missing early gates."""
import copy
import json
from pathlib import Path
import runpy
import unittest

ROOT = Path(__file__).resolve().parents[2]
API = runpy.run_path(str(ROOT / "tools/api_contract.py"))


class ContractControls(unittest.TestCase):
    def setUp(self):
        self.profile = json.loads((ROOT / "docs/plan/api-profile.json").read_text(encoding="utf-8"))
        self.ledger = json.loads((ROOT / "docs/plan/tasks.json").read_text(encoding="utf-8"))

    def validate(self):
        API["validate"](self.profile, self.ledger)

    def test_current_design_and_dependency_order_pass(self):
        self.validate()

    def test_design_cannot_become_fake_runtime_or_measured_evidence(self):
        self.profile["runtime_implemented"] = True
        with self.assertRaisesRegex(ValueError, "implemented runtime"):
            self.validate()
        self.profile["runtime_implemented"] = False
        self.profile["gates"][0]["observed"] = True
        with self.assertRaisesRegex(ValueError, "measured evidence"):
            self.validate()

    def test_excluded_semantics_need_real_alternatives_and_oracles(self):
        row = next(row for row in self.profile["matrix"] if row["kind"] == "unsupported")
        for field in ("alternative", "acceptance", "scalability"):
            previous = row[field]
            row[field] = " "
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.validate()
            row[field] = previous

    def test_unbounded_and_bool_limits_cannot_pass(self):
        for value in (0, -1, True, None, "unlimited"):
            self.profile["example_local_limits"]["max_bulk_bytes"] = value
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "finite positive"):
                self.validate()

    def test_concurrency_cannot_hide_total_work_or_pin_budget(self):
        self.profile["example_local_limits"]["max_inflight_partition_rpcs"] = 65
        with self.assertRaisesRegex(ValueError, "fanout"):
            self.validate()
        self.profile["example_local_limits"]["max_inflight_partition_rpcs"] = 16
        self.profile["required_profile_limits"].remove("global_pin_bytes")
        with self.assertRaisesRegex(ValueError, "retention budget"):
            self.validate()

    def test_unknown_sources_owners_and_duplicate_surfaces_fail(self):
        original = copy.deepcopy(self.profile)
        self.profile["sources"]["bulk"] = "https://example.com/opinion"
        with self.assertRaisesRegex(ValueError, "official OpenSearch"):
            self.validate()
        self.profile = copy.deepcopy(original)
        self.profile["matrix"][0]["owners"] = ["M99.99"]
        with self.assertRaisesRegex(ValueError, "owning tasks"):
            self.validate()
        self.profile = original
        self.profile["matrix"][1]["id"] = self.profile["matrix"][0]["id"]
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            self.validate()

    def test_every_normative_work_and_retention_budget_is_mandatory(self):
        original = self.profile["required_profile_limits"][:]
        for limit in original:
            self.profile["required_profile_limits"] = [name for name in original if name != limit]
            with self.subTest(limit=limit), self.assertRaisesRegex(ValueError, "retention budget"):
                self.validate()

    def test_scale_results_cannot_be_owned_only_by_early_contract_freeze(self):
        gate = next(gate for gate in self.profile["gates"] if gate["id"] == "G06")
        for field, wrong in (("owner", "M02.05"), ("calibration_owner", "M17.03")):
            previous = gate[field]
            gate[field] = wrong
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "real-service qualification"):
                self.validate()
            gate[field] = previous

    def test_removed_admission_dependency_is_not_hidden(self):
        task = next(task for m in self.ledger["milestones"] for task in m["tasks"] if task["id"] == "M07.04")
        task["depends_on"].remove("M07.08")
        with self.assertRaisesRegex(ValueError, "M07.04 -> M07.08"):
            self.validate()

    def test_policy_itself_cannot_drop_the_early_budget_requirement(self):
        self.profile["ordering_constraints"] = [pair for pair in self.profile["ordering_constraints"] if pair != ["M03.03", "M02.08"]]
        with self.assertRaisesRegex(ValueError, "mandatory"):
            self.validate()

    def test_stale_render_cannot_claim_profile_freshness(self):
        text = API["rendered"](self.profile, API["BEGIN"] + "\n" + API["END"])
        API["check_matrix"](self.profile, text)
        self.profile["matrix"][0]["contract"] += " New policy."
        with self.assertRaisesRegex(ValueError, "Stale"):
            API["check_matrix"](self.profile, text)

    def test_plan_red_controls_bind_ids_after_roadmap_reordering(self):
        plan = runpy.run_path(str(ROOT / "docs/plan/tools/plan.py"))
        reordered = copy.deepcopy(self.ledger)
        reordered["milestones"][1], reordered["milestones"][2] = reordered["milestones"][2], reordered["milestones"][1]
        plan["validate"](reordered, files=False)
        self.assertEqual(plan["selftest"](reordered), 17)


if __name__ == "__main__":
    unittest.main()
