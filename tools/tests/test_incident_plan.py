"""The unresolved historical incident cannot be closed by a green rerun alone."""
import copy
import json
from pathlib import Path
import runpy
import unittest

ROOT = Path(__file__).resolve().parents[2]
PLAN = runpy.run_path(str(ROOT / "docs/plan/tools/plan.py"))


class IncidentPlanTests(unittest.TestCase):
    def setUp(self):
        self.data = json.loads((ROOT / "docs/plan/tasks.json").read_text(encoding="utf-8"))
        milestones, self.tasks = PLAN["indexes"](self.data)
        # Keep this draft model independent of later live acceptance states.
        self.tasks["M01.15"]["status"] = "in_progress"
        self.tasks["M01.09"]["status"] = "in_progress"
        self.tasks["M01.08"]["status"] = "planned"
        self.tasks["M02.02"]["status"] = "planned"  # Private preacceptance model only.
        self.tasks["M03.01"]["status"] = "planned"  # Private preacceptance model only.
        self.tasks["M03.02"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        for tid in self.tasks:  # The first runnable slice lands after this draft model.
            if tid.startswith("M20."):
                self.tasks[tid]["status"] = "planned"
        milestones["M20"]["status"] = "planned"
        self.tasks["M01.16"]["status"] = "planned"  # Isolate this pre-disposition fixture from later factual status.
        milestones["M01"]["status"] = "in_progress"

    def reject(self, message):
        with self.assertRaisesRegex(PLAN["InvalidPlan"], message):
            PLAN["validate"](self.data, files=False)

    def accepted_disposition_model(self):
        # This models only graph/state consistency, never actual acceptance.
        evidence = [{"path": "archive/evidence/m01-15-incident-disposition.md",
                     "result": "In-memory acceptance counterexample, not execution evidence."}]
        self.tasks["M01.15"].update(status="done", evidence=copy.deepcopy(evidence))
        self.tasks["M01.09"].update(status="done", evidence=copy.deepcopy(evidence))

    def test_live_draft_does_not_accept_incident_or_agent_baseline(self):
        PLAN["validate"](self.data, files=False)
        self.assertEqual(self.tasks["M01.15"]["status"], "in_progress")
        self.assertEqual(self.tasks["M01.09"]["status"], "in_progress")
        self.assertEqual(self.tasks["M01.08"]["status"], "planned")

    def test_green_prior_evidence_cannot_close_unaccepted_disposition(self):
        self.tasks["M01.09"]["status"] = "done"
        self.reject("Incident disposition is not accepted")

    def test_historical_cause_and_run_identity_cannot_be_invented(self):
        for field, value in (("incident_cause_status", "confirmed"),
                             ("incident_run", "41830170229"),
                             ("acceptance_route", "green-rerun")):
            with self.subTest(field=field):
                previous = self.tasks["M01.09"][field]
                self.tasks["M01.09"][field] = value
                self.reject("retain the unresolved original cause")
                self.tasks["M01.09"][field] = previous

    def test_disposition_keeps_baseline_unqualified_and_provider_blocked(self):
        self.accepted_disposition_model()
        PLAN["validate"](self.data, files=False)
        milestones, tasks = PLAN["indexes"](self.data)
        def ready(task_id):
            return all(tasks[dep]["status"] == "done" for dep in
                       PLAN["task_dependencies"](tasks[task_id], milestones, tasks))
        self.assertTrue(ready("M01.08"))
        self.assertEqual(tasks["M01.08"]["status"], "planned")
        self.assertNotEqual(milestones["M01"]["status"], "done")
        # The prospective trait-start profile is independent of incident
        # closure; it cannot accept the baseline or a real provider.
        self.assertTrue(ready("M03.01"))
        self.assertEqual(tasks["M03.01"]["status"], "planned")
        self.assertTrue(ready("M02.02"))
        self.assertEqual(tasks["M02.02"]["status"], "planned")
        for task_id in ("M02.04", "M03.02"):
            self.assertFalse(ready(task_id), task_id)

    def test_both_tasks_need_the_actual_disposition_record(self):
        self.accepted_disposition_model()
        for task_id in ("M01.09", "M01.15"):
            with self.subTest(task=task_id):
                self.tasks[task_id]["evidence"][0]["path"] = "incident-disposition.md"
                self.reject("acceptance record missing")
                self.tasks[task_id]["evidence"][0]["path"] = "archive/evidence/m01-15-incident-disposition.md"

    def test_replacement_task_and_each_prerequisite_remain_required(self):
        for dep in tuple(self.tasks["M01.15"]["depends_on"]):
            with self.subTest(dependency=dep):
                self.tasks["M01.15"]["depends_on"].remove(dep)
                self.reject("Incident disposition prerequisite missing")
                self.tasks["M01.15"]["depends_on"].append(dep)
        # Find the milestone by identity rather than depending on display order.
        for milestone in self.data["milestones"]:
            milestone["tasks"] = [t for t in milestone["tasks"] if t["id"] != "M01.15"]
        self.reject("Missing incident disposition task")

    def test_baseline_and_product_edges_cannot_be_removed_after_closure(self):
        self.accepted_disposition_model()
        for task_id, dependency in (("M01.08", "M01.09"), ("M02.02", "M19"),
                                    ("M02.02", "M02.09"),
                                    ("M02.04", "M01"), ("M03.02", "M03.01")):
            with self.subTest(task=task_id):
                self.tasks[task_id]["depends_on"].remove(dependency)
                self.reject("Research acceptance gate missing")
                self.tasks[task_id]["depends_on"].append(dependency)


if __name__ == "__main__":
    unittest.main()
