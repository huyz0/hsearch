"""Inventory integrity cannot be confused with reference or service execution."""
import json
import hashlib
import os
from pathlib import Path
import runpy
import shutil
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
API = runpy.run_path(str(ROOT / "tools/api_fixtures.py"))


class FixtureControls(unittest.TestCase):
    def test_real_inventory_does_not_decide_execution(self):
        result = API["check"]()
        self.assertEqual(result["documents"], 11)
        self.assertEqual(result["search_cases"], 14)
        self.assertEqual(result["analyzer_cases"], 4)
        self.assertEqual(result["mutation_histories"], 11)
        self.assertEqual(result["mutation_steps"], 34)
        self.assertEqual(result["profile_areas"], 16)
        self.assertEqual(result["unsupported_inputs"], 14)
        self.assertEqual(result["reference_execution"], "NOT-ASSESSED")
        self.assertEqual(result["hsearch_conformance"], "NOT-ASSESSED")
        self.assertFalse(result["establishes_task_completion"])

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

    def test_rehashed_malformed_histories_and_false_native_claims_still_fail(self):
        for change in ("capture_authority", "unbound", "weak_oracle", "missing_area", "execution_claim"):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                shutil.copytree(ROOT / "fixtures", root / "fixtures")
                (root / "docs/plan").mkdir(parents=True)
                shutil.copyfile(ROOT / "docs/plan/api-profile.json", root / "docs/plan/api-profile.json")
                fixture = root / "fixtures/api/core-v1"
                name = "mutation-cases.json" if change in {"capture_authority", "unbound", "weak_oracle"} else "profile-cases.json"
                value = json.loads((fixture / name).read_text(encoding="utf-8"))
                if change == "capture_authority":value[0]["steps"][0]["capture"] = {"index": "/_id"}
                elif change == "unbound":value[0]["steps"][0]["request"]["path"] = "/{unbound}/_doc/001"
                elif change == "weak_oracle":value[0]["steps"][0]["expect"]["status"] = True
                elif change == "missing_area":value.pop()
                else:value[0]["execution_target"] = "observed_native_success"
                (fixture / name).write_text(json.dumps(value), encoding="utf-8")
                manifest_path = fixture / "manifest.json"
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                manifest["files"][name] = hashlib.sha256((fixture / name).read_bytes()).hexdigest()
                manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
                with self.assertRaises(ValueError):
                    API["check"](root)

    def test_oversized_regular_input_is_refused_before_decode(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            path = root / "oversized.json"
            path.write_bytes(b" " * 65537)
            with self.assertRaisesRegex(ValueError, "bounded regular"):
                API["read"](path, root)

    @unittest.skipUnless(os.name == "posix", "Native FIFO/symlink controls run on Linux")
    def test_nonregular_inputs_are_refused_without_blocking(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            fifo = root / "input.json"
            os.mkfifo(fifo)
            with self.assertRaises(ValueError):
                API["read"](fifo, root)
            (root / "target.json").write_text("{}")
            (root / "alias.json").symlink_to(root / "target.json")
            with self.assertRaises(ValueError):
                API["read"](root / "alias.json", root)


if __name__ == "__main__":
    unittest.main()
