"""Independent uncovered-line and malformed-report controls for coverage accounting."""
from pathlib import Path
import runpy
import unittest

GATE = runpy.run_path(str(Path(__file__).resolve().parents[1] / "coverage_gate.py"))
TYPE, OBSERVE, ENGINE = ("crates/" + name + "/src/lib.rs" for name in ("types", "observe", "engine"))


def fixture():
    sources = {TYPE: {"crate": "types", "sha256": "a" * 64, "line_count": 100},
               OBSERVE: {"crate": "observe", "sha256": "b" * 64, "line_count": 10},
               ENGINE: {"crate": "engine", "sha256": "c" * 64, "line_count": 10}}
    baseline = {TYPE: {line: 0 for line in range(1, 11)}}
    runtime = {TYPE: {**{line: 1 for line in range(1, 11)}, 50: 1000}}
    audit = {path: {"sha256": sources[path]["sha256"], "reason": "Reviewed declaration-only source; no function bodies."} for path in (OBSERVE, ENGINE)}
    return dict(sources=sources, baseline=baseline, runtime=runtime, changed={}, non_executable=audit,
                production_crates=["types", "observe", "engine"])


class CoverageAccountingControls(unittest.TestCase):
    def test_lcov_unicode_separators_are_content_and_cannot_forge_records(self):
        for separator in ['\x85', '\u2028', '\u2029', '\v', '\f', '\r']:
            name = '/original' + separator + 'filename.rs'
            report = 'SF:' + name + '\nDA:1,0\nLF:1\nLH:0\nend_of_record\n'
            self.assertEqual(GATE['parse_lcov'](report), {name: {1: 0}})
            injected = 'SF:/original.rs\nDA:1,0' + separator + 'LF:1\nLH:0\nend_of_record\n'
            with self.assertRaisesRegex(ValueError, 'LCOV integer'):
                GATE['parse_lcov'](injected)

    def test_production_baseline_excludes_test_only_execution_and_reports_honest_na(self):
        data = fixture()
        report = GATE["evaluate"](**data)
        self.assertTrue(report["passed"])
        self.assertEqual(report["aggregate"], {"covered": 10, "coverable": 10, "percent": 100.0, "status": "pass"})
        self.assertEqual(report["runtime_lines_outside_production_baseline"][TYPE], 1)
        self.assertEqual(report["crates"]["observe"]["status"], "not_applicable")
        self.assertIsNone(report["crates"]["observe"]["percent"])
        self.assertEqual(report["changed"]["status"], "not_applicable")

    def test_seeded_uncovered_production_function_cannot_hide_in_test_counts(self):
        data = fixture()
        data["runtime"][TYPE][10] = 0
        data["changed"][TYPE] = [10]
        report = GATE["evaluate"](**data)
        self.assertEqual(report["aggregate"]["percent"], 90)
        self.assertFalse(report["passed"])
        self.assertEqual(report["changed"]["covered"], 0)
        self.assertEqual(report["files"][TYPE]["uncovered_lines"], [10])
        data["runtime"][TYPE][10] = 1
        self.assertTrue(GATE["evaluate"](**data)["passed"])

    def test_aggregate_cannot_hide_a_low_coverage_production_crate(self):
        data = fixture()
        data["non_executable"].pop(ENGINE)
        data["baseline"][ENGINE] = {1: 0}
        data["runtime"][ENGINE] = {1: 0}
        report = GATE["evaluate"](**data)
        self.assertGreater(report["aggregate"]["percent"], 90)
        self.assertFalse(report["passed"])
        self.assertEqual(report["crates"]["engine"]["percent"], 0)

    def test_missing_instrumentation_stale_na_and_executable_exemptions_refuse(self):
        for change in ("missing_map", "stale_hash", "exempt_code", "missing_crate"):
            data = fixture()
            if change == "missing_map": data["baseline"].clear()
            if change == "stale_hash": data["non_executable"][ENGINE]["sha256"] = "d" * 64
            if change == "exempt_code": data["non_executable"][TYPE] = {"sha256": "a" * 64, "reason": "Pretend this executable code has no coverage."}
            if change == "missing_crate": data["sources"].pop(ENGINE); data["non_executable"].pop(ENGINE)
            with self.subTest(change=change), self.assertRaises(ValueError):
                GATE["evaluate"](**data)

    def test_bool_float_out_of_range_counters_and_unowned_sources_refuse(self):
        for change in ("boolean", "float", "outside_lines", "unowned", "nonempty_baseline"):
            data = fixture()
            if change == "boolean": data["runtime"][TYPE][1] = True
            if change == "float": data["runtime"][TYPE][1] = 1.0
            if change == "outside_lines": data["runtime"][TYPE][101] = 1
            if change == "unowned": data["runtime"]["../other.rs"] = {1: 1}
            if change == "nonempty_baseline": data["baseline"][TYPE][1] = 1
            with self.subTest(change=change), self.assertRaises(ValueError):
                GATE["evaluate"](**data)

    def test_changed_test_only_line_does_not_become_production_coverage(self):
        data = fixture()
        data["changed"][TYPE] = [50]
        report = GATE["evaluate"](**data)
        self.assertEqual(report["changed"]["coverable"], 0)
        self.assertIsNone(report["changed"]["percent"])


class LcovParserControls(unittest.TestCase):
    def test_excluded_tool_output_does_not_supply_denominator_or_bypass_production_checks(self):
        production = "SF:/original.rs\nDA:1,1\nLF:1\nLH:1\nend_of_record\n"
        excluded = "SF:/dependency.rs\nDA:1,0\nLF:2\nLH:0\nend_of_record\n"
        scope = lambda name: name == "/original.rs"
        self.assertEqual(GATE["parse_lcov"](production + excluded, include=scope), {"/original.rs": {1: 1}})
        with self.assertRaises(ValueError):
            GATE["parse_lcov"](production.replace("LF:1", "LF:2") + excluded, include=scope)
        with self.assertRaises(ValueError):
            GATE["parse_lcov"](production + excluded.replace("end_of_record\n", ""), include=scope)

    def test_exact_line_counters_and_summary_are_parsed(self):
        report = "SF:/original/fixture.rs\nFN:1,original\nDA:1,2\nDA:3,0\nLF:2\nLH:1\nend_of_record\n"
        self.assertEqual(GATE["parse_lcov"](report), {"/original/fixture.rs": {1: 2, 3: 0}})

    def test_truncated_duplicate_invalid_counter_and_inconsistent_summaries_refuse(self):
        valid = "SF:/original.rs\nDA:1,1\nLF:1\nLH:1\nend_of_record\n"
        bad = [valid.replace("end_of_record\n", ""), valid + valid,
               valid.replace("DA:1,1", "DA:1,true"), valid.replace("DA:1,1", "DA:0,1"),
               valid.replace("DA:1,1", "DA:1,1\nDA:1,1"), valid.replace("LF:1", "LF:2"),
               valid.replace("LH:1", "LH:0"), valid.replace("DA:1,1", "UNKNOWN:1,1")]
        for report in bad:
            with self.subTest(report=report), self.assertRaises(ValueError):
                GATE["parse_lcov"](report)
