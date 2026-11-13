"""Planning research artifacts must remain reproducible during implementation."""
import json
from pathlib import Path
import runpy
import unittest

ROOT = Path(__file__).resolve().parents[2]
RESEARCH = ROOT / "docs/plan/tools/cost_research.py"


class ResearchReproduction(unittest.TestCase):
    def test_recorded_experiments_match_the_executable_model(self):
        module = runpy.run_path(str(RESEARCH))
        data = module["run"]()
        directory = ROOT / "docs/plan/archive/evidence"
        self.assertEqual(json.loads((directory / "cost-research-results.json").read_text(encoding="utf-8")), data)
        self.assertEqual((directory / "cost-research-results.md").read_text(encoding="utf-8"), module["summary"](data))
        self.assertEqual(len(data["rows"]), 72)
        self.assertTrue(data["fencing_model"]["serialized_poisoned_retained"]["safe"])
        for name, result in data["fencing_model"].items():
            if name.startswith("BUG"):
                self.assertFalse(result["safe"], name)


if __name__ == "__main__":
    unittest.main()
