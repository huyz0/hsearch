"""Explicit native export profiles preserve physical and source authority controls."""
import copy
import json
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
REPORTS = runpy.run_path(str(ROOT / "tools/coverage_reports.py"))
COV = runpy.run_path(str(ROOT / "tools/coverage.py"))


def report(filename="/source/lib.rs"):
    return {"type": "llvm.coverage.json.export", "version": "3.1.0", "data": [{"files": [{
        "filename": filename, "branches": [], "expansions": [], "mcdc_records": [],
        "segments": [[1, 1, 1, True, True, False]],
        "summary": {"lines": {"count": 1, "covered": 1, "percent": 100}}}], "totals": {}}]}


class NativeExportProfiles(unittest.TestCase):
    def test_compact_needs_explicit_profile_while_legacy_full_stays_strict(self):
        compact = report()
        full = copy.deepcopy(compact)
        full["data"][0]["functions"] = []
        with self.assertRaisesRegex(ValueError, "Native export shape"):
            REPORTS["decode"](json.dumps(compact))
        with self.assertRaisesRegex(ValueError, "Native export shape"):
            REPORTS["decode"](json.dumps(full), profile="files")
        a = REPORTS["decode"](json.dumps(full))
        b = REPORTS["decode"](json.dumps(compact), profile="files")
        self.assertEqual(a.files, b.files)
        self.assertNotEqual(a.raw_sha256, b.raw_sha256)
        self.assertEqual(REPORTS["project"](a, {"/source/lib.rs"}), {"/source/lib.rs": {1: 1}})
        self.assertEqual(REPORTS["project"](b, {"/source/lib.rs"}), {"/source/lib.rs": {1: 1}})

    def test_compact_cannot_drop_expansions_segments_or_filename(self):
        for key in ["filename", "segments", "expansions", "branches", "mcdc_records", "summary"]:
            candidate = report()
            del candidate["data"][0]["files"][0][key]
            with self.subTest(key=key), self.assertRaises(ValueError):
                REPORTS["decode"](json.dumps(candidate), profile="files")

    def test_compact_preserves_duplicate_and_segment_type_refusals(self):
        duplicate = report()
        duplicate["data"][0]["files"] *= 2
        with self.assertRaisesRegex(ValueError, "duplicate native filename"):
            REPORTS["decode"](json.dumps(duplicate), profile="files")
        malformed = report()
        malformed["data"][0]["files"][0]["segments"][0][3] = 1
        with self.assertRaisesRegex(ValueError, "Native segment shape"):
            REPORTS["decode"](json.dumps(malformed), profile="files")
        for profile in [None, [], "unknown"]:
            with self.subTest(profile=profile), self.assertRaisesRegex(ValueError, "Unknown native export profile"):
                REPORTS["decode"](json.dumps(report()), profile=profile)

    def test_compact_cannot_hide_an_unowned_external_source(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory).resolve()
            root = parent / "checkout"
            root.mkdir()
            encoded = json.dumps(report((parent / "unowned.rs").as_posix()))
            with patch.dict(COV["normalize_report"].__globals__, ROOT=root):
                with self.assertRaisesRegex(ValueError, "Unowned external"):
                    COV["normalize_report"](encoded, {}, {}, [], set(), export_profile="files")

    def test_export_keeps_all_inventoried_objects_without_filename_filters(self):
        llvm = Path("/pinned/llvm")
        objects = [Path("/owned/a.o"), Path("/owned/b.o"), Path("/owned/test-binary")]
        for profile in [None, Path("/owned/merged.profdata")]:
            args = COV["native_export_command"](llvm, objects, profile_path=profile)
            self.assertIn("--skip-functions", args)
            self.assertEqual([x for x in args if x in objects], objects)
            self.assertEqual(args.count("--object"), 2)
            self.assertFalse(any(str(x).startswith(("--ignore-filename", "--sources", "--summary-only", "--skip-expansions")) for x in args))
            self.assertEqual("--empty-profile" in args, profile is None)
            self.assertEqual(any(str(x).startswith("--instr-profile=") for x in args), profile is not None)
        with self.assertRaisesRegex(ValueError, "every inventoried object"):
            COV["native_export_command"](llvm, [])


if __name__ == "__main__":
    unittest.main()
