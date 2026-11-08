"""Graph counterexamples for research boundaries and the narrow trait-start policy."""
import copy
import json
from pathlib import Path
import runpy
import unittest

ROOT = Path(__file__).resolve().parents[2]
PLAN = runpy.run_path(str(ROOT / "docs/plan/tools/plan.py"))


class ResearchPlanTests(unittest.TestCase):
    def setUp(self):
        self.data = json.loads((ROOT / "docs/plan/tasks.json").read_text(encoding="utf-8"))
        # Construct an unqualified-baseline model, independent of future live
        # acceptance states. This fixture is never written back to the ledger.
        milestones, tasks = PLAN["indexes"](self.data)
        closure = set()
        def include(task_id):
            if task_id in closure:
                return
            closure.add(task_id)
            for dependency in tasks[task_id]["depends_on"]:
                children = ([t["id"] for t in milestones[dependency]["tasks"]]
                            if dependency in milestones else [dependency])
                for child in children:
                    include(child)
        include("M02.09")
        include("M02.10")
        closure -= {"M01.14", "M02.09", "M02.10"}
        self.assertTrue({"M01.08", "M01.09"}.isdisjoint(closure))
        for task_id, task in tasks.items():
            task["status"] = "done" if task_id in closure else "planned"
            if task["status"] == "done" and not task["evidence"]:
                task["evidence"] = [{"path": "coordinator-research.md",
                                    "result": "In-memory prerequisite model only."}]
        tasks["M01.09"]["status"] = "in_progress"
        tasks["M01.14"]["status"] = "in_progress"
        for milestone in milestones.values():
            milestone["status"] = ("done" if all(t["status"] == "done"
                                    for t in milestone["tasks"]) else "planned")

    def graph(self, data):
        return PLAN["indexes"](data)

    def ready(self, data, task_id):
        milestones, tasks = self.graph(data)
        return all(tasks[dep]["status"] == "done" for dep in
                   PLAN["task_dependencies"](tasks[task_id], milestones, tasks))

    def completed_research(self):
        data = copy.deepcopy(self.data)
        _, tasks = self.graph(data)
        for task_id in ("M01.14", "M02.09", "M02.10"):
            tasks[task_id].update(status="done", evidence=[{
                "path": "coordinator-research.md",
                "result": "In-memory graph counterexample only; never actual acceptance."}])
        PLAN["validate"](data, files=False)
        return data

    def test_current_plan_retains_unresolved_baseline(self):
        PLAN["validate"](self.data, files=False)
        _, tasks = self.graph(self.data)
        self.assertEqual(tasks["M01.09"]["status"], "in_progress")
        self.assertEqual(tasks["M01.08"]["status"], "planned")
        self.assertFalse(self.ready(self.data, "M02.09"))
        self.assertFalse(self.ready(self.data, "M02.10"))

    def test_accepted_research_preserves_the_independent_trait_start(self):
        data = copy.deepcopy(self.data)
        _, tasks = self.graph(data)
        tasks["M01.14"].update(status="done", evidence=[{
            "path": "coordinator-research.md", "result": "In-memory readiness model."}])
        PLAN["validate"](data, files=False)
        frontier = {"M02.09", "M02.10", "M02.02", "M02.04", "M03.01", "M01.08"}
        ready = {tid for tid in frontier
                 if tasks[tid]["status"] == "planned" and self.ready(data, tid)}
        self.assertEqual(ready, {"M02.09", "M02.10", "M03.01"})

    def test_completed_research_opens_engine_start_without_accepting_products(self):
        self.assertFalse(self.ready(self.data, "M02.02"))  # M02.09 is incomplete.
        data = self.completed_research()
        milestones, tasks = self.graph(data)
        self.assertTrue(self.ready(data, "M02.02"))
        self.assertEqual(tasks["M02.02"]["status"], "planned")
        self.assertTrue(self.ready(data, "M03.01"))
        self.assertEqual(tasks["M03.01"]["status"], "planned")
        self.assertEqual(tasks["M01.08"]["status"], "planned")
        self.assertNotEqual(milestones["M02"]["status"], "done")
        self.assertNotEqual(milestones["M03"]["status"], "done")
        tasks["M02.09"]["status"] = "planned"
        self.assertFalse(self.ready(data, "M02.02"))
        self.assertFalse(self.ready(data, "M02.04"))

    def test_removed_whole_baseline_is_a_real_false_readiness_counterexample(self):
        for task_id in ("M02.04",):
            with self.subTest(task=task_id):
                data = self.completed_research()
                _, tasks = self.graph(data)
                tasks[task_id]["depends_on"].remove("M01")
                self.assertTrue(self.ready(data, task_id))
                with self.assertRaisesRegex(PLAN["InvalidPlan"], "Research acceptance gate missing"):
                    PLAN["validate"](data, files=False)

    def test_each_engine_start_prerequisite_prevents_real_false_readiness(self):
        for dependency in ("M19", "M02.09"):
            with self.subTest(dependency=dependency):
                data = self.completed_research()
                milestones, tasks = self.graph(data)
                children = ([t["id"] for t in milestones[dependency]["tasks"]]
                            if dependency in milestones else [dependency])
                for child in children:
                    tasks[child]["status"] = "planned"
                self.assertFalse(self.ready(data, "M02.02"))
                tasks["M02.02"]["depends_on"].remove(dependency)
                self.assertTrue(self.ready(data, "M02.02"))
                with self.assertRaisesRegex(PLAN["InvalidPlan"], "Research acceptance gate missing"):
                    PLAN["research_gate_contract"](milestones, tasks)

    def test_retired_engine_hold_cannot_silently_block_admitted_start(self):
        data = self.completed_research()
        _, tasks = self.graph(data)
        self.assertTrue(self.ready(data, "M02.02"))
        tasks["M02.02"]["depends_on"].append("M01")
        self.assertFalse(self.ready(data, "M02.02"))
        with self.assertRaisesRegex(PLAN["InvalidPlan"], "Native engine start profile changed"):
            PLAN["validate"](data, files=False)

    def test_other_original_product_and_incident_edges_are_required(self):
        for task_id, dep in (("M07.02", "M01"), ("M16.04", "M01"),
                             ("M02.04", "M02.10"),
                             ("M01.08", "M01.09"), ("M02.03", "M02.02"),
                             ("M02.05", "M02.04"), ("M02.06", "M02.05"),
                             ("M02.07", "M02.04")):
            with self.subTest(task=task_id, dependency=dep):
                data = self.completed_research()
                _, tasks = self.graph(data)
                tasks[task_id]["depends_on"].remove(dep)
                with self.assertRaisesRegex(PLAN["InvalidPlan"], "Research acceptance gate missing"):
                    PLAN["validate"](data, files=False)

    def test_all_original_prerequisites_remain_required(self):
        omitted = [
            ("M01.08", "M01.04"), ("M01.08", "M01.05"),
            ("M01.08", "M01.06"), ("M01.08", "M01.07"),
            ("M01.08", "M19"), ("M07.02", "M05"),
            ("M16.04", "M08"), ("M16.04", "M16.02"),
            ("M02.05", "M02.01")]
        for task_id, dependency in omitted:
            with self.subTest(task=task_id, dependency=dependency):
                data = self.completed_research()
                _, tasks = self.graph(data)
                tasks[task_id]["depends_on"].remove(dependency)
                with self.assertRaisesRegex(PLAN["InvalidPlan"], "Research acceptance gate missing"):
                    PLAN["validate"](data, files=False)

    def test_trait_start_is_independent_of_the_unqualified_baseline(self):
        PLAN["validate"](self.data, files=False)
        milestones, tasks = self.graph(self.data)
        self.assertEqual(tasks["M03.01"]["depends_on"],
                         ["M19", "M01.01", "M01.02", "M01.03",
                          "M01.05", "M01.06", "M01.07"])
        self.assertTrue(self.ready(self.data, "M03.01"))
        self.assertEqual(tasks["M03.01"]["status"], "planned")
        self.assertEqual(tasks["M01.08"]["status"], "planned")
        self.assertNotEqual(milestones["M01"]["status"], "done")
        self.assertNotEqual(milestones["M03"]["status"], "done")

    def test_each_trait_start_edge_prevents_a_real_false_readiness(self):
        # Mutate only a private readiness model; unrelated completed tasks can
        # share this prerequisite, so validate the edge contract separately.
        for dependency in ("M19", "M01.01", "M01.02", "M01.03",
                           "M01.05", "M01.06", "M01.07"):
            with self.subTest(dependency=dependency):
                data = self.completed_research()
                milestones, tasks = self.graph(data)
                children = ([t["id"] for t in milestones[dependency]["tasks"]]
                            if dependency in milestones else [dependency])
                for child in children:
                    tasks[child]["status"] = "planned"
                self.assertFalse(self.ready(data, "M03.01"))
                tasks["M03.01"]["depends_on"].remove(dependency)
                self.assertTrue(self.ready(data, "M03.01"))
                with self.assertRaisesRegex(PLAN["InvalidPlan"],
                                           "Product start profile changed"):
                    PLAN["research_gate_contract"](milestones, tasks)

    def test_trait_start_profile_cannot_silently_add_another_gate(self):
        data = self.completed_research()
        _, tasks = self.graph(data)
        tasks["M03.01"]["depends_on"].append("M01")
        with self.assertRaisesRegex(PLAN["InvalidPlan"],
                                   "Product start profile changed"):
            PLAN["validate"](data, files=False)

    def test_accepted_traits_allow_probe_start_without_qualifying_provider_or_milestone(self):
        data = self.completed_research()
        milestones, tasks = self.graph(data)
        self.assertFalse(self.ready(data, "M03.02"))
        tasks["M03.01"].update(status="done", evidence=[{
            "path": "product-start-policy.md",
            "result": "In-memory trait acceptance model only."}])
        PLAN["validate"](data, files=False)
        self.assertTrue(self.ready(data, "M03.02"))
        self.assertEqual(tasks["M03.02"]["status"], "planned")
        self.assertNotEqual(milestones["M03"]["status"], "done")
        self.assertEqual(tasks["M01.08"]["status"], "planned")
        # Adding the retired hold really prevents otherwise admitted probe work.
        tasks["M03.02"]["depends_on"].append("M01")
        self.assertFalse(self.ready(data, "M03.02"))
        with self.assertRaisesRegex(PLAN["InvalidPlan"], "Provider start profile changed"):
            PLAN["validate"](data, files=False)
        tasks["M03.02"]["depends_on"].remove("M01")
        # Removing the accepted-trait edge creates false readiness and must fail.
        tasks["M03.01"]["status"] = "planned"
        self.assertFalse(self.ready(data, "M03.02"))
        tasks["M03.02"]["depends_on"].remove("M03.01")
        self.assertTrue(self.ready(data, "M03.02"))
        with self.assertRaisesRegex(PLAN["InvalidPlan"], "Research acceptance gate missing"):
            PLAN["validate"](data, files=False)

    def test_storage_milestone_and_later_format_runtime_gates_remain(self):
        data = self.completed_research()
        milestones, _ = self.graph(data)
        milestones["M03"]["depends_on"].remove("M01")
        with self.assertRaisesRegex(PLAN["InvalidPlan"],
                                   "Product acceptance gate missing"):
            PLAN["validate"](data, files=False)
        edges = {
            "M03.03": ("M03.01", "M19", "M02.08", "M02.05"),
            "M03.04": ("M03.03",),
            "M03.07": ("M02.07", "M03.01"),
            "M03.08": ("M02.07", "M03.04", "M03.02", "M03.07"),
            "M03.05": ("M03.02", "M03.04", "M03.08"),
            "M03.06": ("M03.05", "M03.07"),
        }
        for task_id, dependencies in edges.items():
            for dependency in dependencies:
                with self.subTest(task=task_id, dependency=dependency):
                    data = self.completed_research()
                    _, tasks = self.graph(data)
                    tasks[task_id]["depends_on"].remove(dependency)
                    with self.assertRaisesRegex(PLAN["InvalidPlan"],
                                               "Research acceptance gate missing"):
                        PLAN["validate"](data, files=False)

    def test_research_scope_cannot_be_silently_widened(self):
        for task_id in ("M02.09", "M02.10"):
            data = copy.deepcopy(self.data)
            _, tasks = self.graph(data)
            tasks[task_id]["execution_scope"] = "delegated-product"
            with self.assertRaisesRegex(PLAN["InvalidPlan"], "Research execution scope changed"):
                PLAN["validate"](data, files=False)

    def test_research_prerequisites_and_milestone_completion_are_retained(self):
        for task_id, dependency in (("M02.09", "M01.14"), ("M02.10", "M08.01")):
            data = copy.deepcopy(self.data)
            _, tasks = self.graph(data)
            tasks[task_id]["depends_on"].remove(dependency)
            with self.assertRaisesRegex(PLAN["InvalidPlan"], "Research acceptance gate missing"):
                PLAN["validate"](data, files=False)
        data = self.completed_research()
        milestones, _ = self.graph(data)
        milestones["M02"]["depends_on"].remove("M01")
        with self.assertRaisesRegex(PLAN["InvalidPlan"], "Research acceptance gate missing"):
            PLAN["validate"](data, files=False)

        data = self.completed_research()
        milestones, _ = self.graph(data)
        milestones["M02"]["status"] = "done"
        with self.assertRaisesRegex(PLAN["InvalidPlan"], "Milestone/task completion mismatch"):
            PLAN["validate"](data, files=False)


if __name__ == "__main__":
    unittest.main()
