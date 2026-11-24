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
        self.tasks["M02.03"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        self.tasks["M02.12"]["status"] = "planned"  # Depends on M02.02; keep this model pre-acceptance.
        self.tasks["M02.06"]["status"] = "planned"  # Depends on M02.03, M02.05, M02.07 and M02.12 above.
        self.tasks["M03.01"]["status"] = "planned"  # Private preacceptance model only.
        self.tasks["M03.02"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        for tid in self.tasks:  # The first runnable slice lands after this draft model.
            if tid.startswith("M20."):
                self.tasks[tid]["status"] = "planned"
        milestones["M20"]["status"] = "planned"
        self.tasks["M24.01"]["status"] = "planned"  # Depends on M20.04; keep this model pre-acceptance.
        self.tasks["M21.01"]["status"] = "planned"  # Depends on M20.04; keep this model pre-acceptance.
        self.tasks["M21.02"]["status"] = "planned"  # Depends on M20.04; keep this model pre-acceptance.
        self.tasks["M21.03"]["status"] = "planned"  # Depends on M21.02; keep this model pre-acceptance.
        self.tasks["M21.08"]["status"] = "planned"  # Depends on M21.01; keep this model pre-acceptance.
        self.tasks["M21.07"]["status"] = "planned"  # Depends on M21.03; keep this model pre-acceptance.
        self.tasks["M24.02"]["status"] = "planned"  # Depends on M24.01; keep this model pre-acceptance.
        self.tasks["M24.03"]["status"] = "planned"  # Depends on M24.01; keep this model pre-acceptance.
        self.tasks["M03.10"]["status"] = "planned"  # Depends on M03.02; keep this model pre-acceptance.
        self.tasks["M02.10"]["status"] = "planned"  # Keep this model pre-acceptance.
        self.tasks["M02.04"]["status"] = "planned"  # Depends on M02.10; keep the model pre-acceptance.
        self.tasks["M03.09"]["status"] = "planned"  # Depends on M02.07; keep the model pre-acceptance.
        self.tasks["M02.07"]["status"] = "planned"  # Depends on M02.04; keep the model pre-acceptance.
        self.tasks["M02.05"]["status"] = "planned"  # Depends on M02.07; keep the model pre-acceptance.
        self.tasks["M03.03"]["status"] = "planned"  # Depends on M03.09; keep the model pre-acceptance.
        self.tasks["M03.04"]["status"] = "planned"  # Depends on M03.03; keep the model pre-acceptance.
        self.tasks["M03.07"]["status"] = "planned"  # Depends on M02.07 and M03.01; keep the model pre-acceptance.
        self.tasks["M03.12"]["status"] = "planned"  # Depends on M03.07; keep the model pre-acceptance.
        self.tasks["M03.11"]["status"] = "planned"  # Depends on M03.10 above; keep the model pre-acceptance.
        self.tasks["M03.14"]["status"] = "planned"  # Depends on M03.10 above; keep the model pre-acceptance.
        self.tasks["M04.01"]["status"] = "planned"  # Depends on M03.01 and M03.02 above; keep the model pre-acceptance.
        self.tasks["M04.02"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        self.tasks["M04.03"]["status"] = "planned"  # Depends on M04.01 above; keep the model pre-acceptance.
        self.tasks["M03.13"]["status"] = "planned"  # Depends on M03.10 and M03.12 above; keep the model pre-acceptance.
        self.tasks["M04.04"]["status"] = "planned"  # Depends on M04.03 above; keep the model pre-acceptance.
        self.tasks["M04.05"]["status"] = "planned"  # Depends on M04.04 above; keep the model pre-acceptance.
        self.tasks["M04.06"]["status"] = "planned"  # Depends on M04.05 above; keep the model pre-acceptance.
        self.tasks["M06.01"]["status"] = "planned"  # Depends on M04 and M05 above; keep the model pre-acceptance.
        self.tasks["M06.02"]["status"] = "planned"  # Depends on M06.01 above; keep the model pre-acceptance.
        self.tasks["M06.09"]["status"] = "planned"  # Depends on M06.01 above; keep the model pre-acceptance.
        self.tasks["M06.04"]["status"] = "planned"  # Depends on M06.02 above; keep the model pre-acceptance.
        self.tasks["M06.03"]["status"] = "planned"  # Depends on M06.02 above; keep the model pre-acceptance.
        self.tasks["M06.10"]["status"] = "planned"  # Depends on M06.03 above; keep the model pre-acceptance.
        self.tasks["M06.11"]["status"] = "planned"  # Depends on M06.10 above; keep the model pre-acceptance.
        self.tasks["M06.12"]["status"] = "planned"  # Depends on M06.11 above; keep the model pre-acceptance.
        self.tasks["M06.13"]["status"] = "planned"  # Depends on M06.10 above; keep the model pre-acceptance.
        self.tasks["M06.14"]["status"] = "planned"  # Depends on M06.12 above; keep the model pre-acceptance.
        self.tasks["M06.15"]["status"] = "planned"  # Depends on M06.14 above; keep the model pre-acceptance.
        self.tasks["M06.17"]["status"] = "planned"  # Depends on M06.14 above; keep the model pre-acceptance.
        self.tasks["M06.16"]["status"] = "planned"  # Depends on M06.15 and M06.17 above; keep the model pre-acceptance.
        self.tasks["M06.18"]["status"] = "planned"  # Depends on M06.13 and M06.14 above; keep the model pre-acceptance.
        self.tasks["M06.22"]["status"] = "planned"  # Depends on M06.16 above; keep the model pre-acceptance.
        self.tasks["M06.19"]["status"] = "planned"  # Depends on M06.16, M06.18 and M06.22 above; keep the model pre-acceptance.
        self.tasks["M06.20"]["status"] = "planned"  # Depends on M06.14 and M06.17 above; keep the model pre-acceptance.
        self.tasks["M05.01"]["status"] = "planned"  # Depends on M03.01 and M03.09 above; keep the model pre-acceptance.
        self.tasks["M05.02"]["status"] = "planned"  # Depends on M03.01 and M03.09 above; keep the model pre-acceptance.
        self.tasks["M05.03"]["status"] = "planned"  # Depends on M05.02 above; keep the model pre-acceptance.
        self.tasks["M05.04"]["status"] = "planned"  # Depends on M05.03 above; keep the model pre-acceptance.
        self.tasks["M05.05"]["status"] = "planned"  # Depends on M05.04 above; keep the model pre-acceptance.
        self.tasks["M05.06"]["status"] = "planned"  # Depends on M05.01 and M05.03 above; keep the model pre-acceptance.
        self.tasks["M05.08"]["status"] = "planned"  # Depends on M05.02 above; keep the model pre-acceptance.
        self.tasks["M05.07"]["status"] = "planned"  # Depends on M05.05 and M05.06 above; keep the model pre-acceptance.
        for tid in ("M03.05", "M03.06", "M03.08"):  # Done later; keep this model pre-acceptance.
            self.tasks[tid]["status"] = "planned"
        milestones["M03"]["status"] = "planned"
        milestones["M05"]["status"] = "planned"  # Its tasks are reset above; M05 is done in the live plan.
        self.tasks["M13.01"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        self.tasks["M13.02"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        self.tasks["M07.02"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        self.tasks["M23.01"]["status"] = "planned"  # Depends on M02.12 above; keep this model pre-acceptance.
        self.tasks["M23.02"]["status"] = "planned"  # Depends on M23.01 above; keep this model pre-acceptance.
        self.tasks["M23.03"]["status"] = "planned"  # Depends on M23.02 above; keep this model pre-acceptance.
        self.tasks["M23.04"]["status"] = "planned"  # Depends on M23.02 above; keep this model pre-acceptance.
        self.tasks["M23.05"]["status"] = "planned"  # Depends on M23.03 above; keep this model pre-acceptance.
        self.tasks["M23.06"]["status"] = "planned"  # Depends on M23.03 above; keep this model pre-acceptance.
        milestones["M23"]["status"] = "planned"  # Its tasks are reset above; M23 is done in the live plan.
        self.tasks["M21.09"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        self.tasks["M21.10"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        self.tasks["M21.04"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        self.tasks["M21.05"]["status"] = "planned"  # Done later; keep this model pre-acceptance.
        self.tasks["M22.05"]["status"] = "planned"  # Depends on M03.05 above; keep the model pre-acceptance.
        self.tasks["M01.16"]["status"] = "planned"  # Isolate this pre-disposition fixture from later factual status.
        milestones["M01"]["status"] = "in_progress"
        milestones["M04"]["status"] = "planned"  # Its tasks are reset above; M04 is done in the live plan.
        milestones["M02"]["status"] = "planned"  # Its tasks are reset above; M02 is done in the live plan.

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
