"""Inventory integrity cannot be confused with reference or service execution."""
import json
from pathlib import Path
import runpy
import shutil
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
API = runpy.run_path(str(ROOT / "tools/api_fixtures.py"))


class FixtureControls(unittest.TestCase):
    def test_real_inventory_retains_missing_execution(self):
        result = API["check"]()
        self.assertEqual(result["documents"], 11)
        self.assertEqual(result["search_cases"], 13)
        self.assertEqual(result["analyzer_cases"], 4)
        self.assertEqual(result["reference_execution"], "NOT-RUN")
        self.assertEqual(result["hsearch_conformance"], "NOT-RUN")
        self.assertFalse(result["task_completion"])

    def test_tampered_dataset_stale_profile_and_invented_execution_fail(self):
        for change in ("dataset", "profile", "reference_claim", "hsearch_claim", "path"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                shutil.copytree(ROOT / "fixtures", root / "fixtures")
                (root / "docs/plan").mkdir(parents=True)
                shutil.copyfile(ROOT / "docs/plan/api-profile.json", root / "docs/plan/api-profile.json")
                manifest_path = root / "fixtures/api/core-v1/manifest.json"
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                if change == "dataset":
                    (root / "fixtures/api/core-v1/documents.json").write_text("[]\n")
                elif change == "profile":
                    manifest["profile_sha256"] = "0" * 64
                elif change == "reference_claim":
                    manifest["reference"]["reference_observed"] = True
                elif change == "hsearch_claim":
                    manifest["hsearch_runtime_implemented"] = True
                else:
                    manifest["files"]["../private.json"] = "0" * 64
                manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
                with self.assertRaises(ValueError):
                    API["check"](root)

    def test_duplicate_json_metadata_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "Duplicate JSON key"):
            json.loads('{"reference_observed":false,"reference_observed":true}', object_pairs_hook=API["pairs"])


if __name__ == "__main__":
    unittest.main()
