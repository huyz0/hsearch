"""Negative controls for the bootstrap's coverage threshold and report presence."""
import copy
from pathlib import Path
import runpy
import unittest

CHECK = runpy.run_path(str(Path(__file__).resolve().parents[1] / "coverage_check.py"))["check"]


class CoverageControls(unittest.TestCase):
    def setUp(self):
        self.report = {"data": [{"files": [{"filename": "/work/crates/types/src/lib.rs",
            "summary": {"lines": {"count": 100, "covered": 90}}}]}]}

    def test_threshold_is_inclusive(self):
        self.assertEqual(CHECK(self.report)["types"], [90, 100])

    def test_uncovered_production_path_fails(self):
        self.report["data"][0]["files"][0]["summary"]["lines"]["covered"] = 89
        with self.assertRaisesRegex(ValueError, "Below 90%"):
            CHECK(self.report)

    def test_missing_and_duplicate_reports_fail(self):
        with self.assertRaisesRegex(ValueError, "Missing production"):
            CHECK({"data": [{"files": []}]})
        self.report["data"][0]["files"] *= 2
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            CHECK(self.report)

    def test_each_production_file_is_checked(self):
        extra = copy.deepcopy(self.report["data"][0]["files"][0])
        extra["filename"] = "/work/crates/engine/src/lib.rs"
        extra["summary"]["lines"]["covered"] = 1
        self.report["data"][0]["files"].append(extra)
        with self.assertRaisesRegex(ValueError, "Below 90%"):
            CHECK(self.report)

    def test_invalid_counts_fail(self):
        for total, covered in ((0, 1), (100, -1), (100, 101), (True, 1), (100, 99.5)):
            self.report["data"][0]["files"][0]["summary"]["lines"] = {"count": total, "covered": covered}
            with self.assertRaisesRegex(ValueError, "Invalid coverage"):
                CHECK(self.report)

    def test_explicit_selected_scope_preserves_threshold(self):
        self.assertEqual(CHECK(self.report, ("types",)), {"types": [90, 100]})
        self.report["data"][0]["files"][0]["summary"]["lines"]["covered"] = 89
        with self.assertRaisesRegex(ValueError, "Below 90%"):
            CHECK(self.report, ("types",))
        for scope in ((), ("unknown",)):
            with self.assertRaisesRegex(ValueError, "scope"):
                CHECK(self.report, scope)

    def test_empty_engine_scope_is_na_but_executable_engine_lines_keep_floor(self):
        self.assertEqual(CHECK({"data": [{"files": []}]}, ("engine",)), {"engine": [0, 0]})
        report = copy.deepcopy(self.report)
        report["data"][0]["files"][0]["filename"] = "/work/crates/engine/src/lib.rs"
        report["data"][0]["files"][0]["summary"]["lines"]["covered"] = 89
        with self.assertRaisesRegex(ValueError, "Below 90%"):
            CHECK(report, ("engine",))


if __name__ == "__main__":
    unittest.main()
