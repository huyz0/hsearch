"""Regression controls for immutable public-to-native fixture projection."""
import copy
import json
from pathlib import Path
import runpy
import shutil
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
R = runpy.run_path(str(ROOT / "tools/engine_research.py"))


class NativeFixtureProjection(unittest.TestCase):
    def fixture_copy(self, root):
        for folder in ["fixtures/api/core-v1", "fixtures/engine-research", "docs/plan/research"]:
            shutil.copytree(ROOT / folder, root / folder)

    def test_frozen_request_perturbation_cannot_reuse_reference_oracle(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            self.fixture_copy(root)
            path = root / "fixtures/api/core-v1/search-cases.json"
            cases = json.loads(path.read_text(encoding="utf-8"))
            cases[0]["request"]["json"]["query"] = {"term": {"category": "unknown"}}
            path.write_text(json.dumps(cases), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Frozen fixture changed"):
                R["render"](root)

    def test_held_out_order_oracle_cannot_be_replaced_with_conjunction(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            self.fixture_copy(root)
            path = root / "fixtures/engine-research/held-out.json"
            data = json.loads(path.read_text(encoding="utf-8"))
            data["phrase"]["exact_phrase_ids"] = data["phrase"]["unsafe_unordered_and_ids"]
            path.write_text(json.dumps(data), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Frozen held-out controls changed"):
                R["render"](root)

    def test_range_translation_rejects_overflow_and_preserves_exclusive_bound(self):
        actual = R["clause"]({"range": {"price": {"gte": 10, "lt": 30}}})
        self.assertIn('"price", 10, 29', actual)
        for value in [{"gt": 2**63 - 1}, {"lt": -(2**63)}, {"gte": True}, {"gte": 2**63}]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                R["clause"]({"range": {"price": value}})

    def test_unknown_unbounded_and_text_ranges_are_refused(self):
        query = {"term": {"category": "book"}}
        deep = copy.deepcopy(query)
        for _ in range(10):
            deep = {"bool": {"must": [deep]}}
        for candidate in [{"script": {}}, {"range": {"title": {"gte": "a"}}}, {"terms": {"category": ["book"] * 17}}, {"bool": {"should": [query] * 17}}, deep]:
            with self.subTest(candidate=candidate), self.assertRaises(ValueError):
                R["clause"](candidate)


if __name__ == "__main__":
    unittest.main()
