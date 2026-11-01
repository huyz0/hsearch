"""Independent framing, physical-coordinate and retained native-report controls."""
import copy
import hashlib
import json
from pathlib import Path
import random
import runpy
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
N = runpy.run_path(str(ROOT / "tools/coverage_reports.py"))
GATE = runpy.run_path(str(ROOT / "tools/coverage_gate.py"))


def file(name="/source/lib.rs", segments=None, count=1, covered=0):
    return {"filename": name, "segments": segments or [], "branches": [],
            "expansions": [], "mcdc_records": [], "summary": {"lines": {
                "count": count, "covered": covered, "percent": 0 if count == 0 else 100 * covered / count}}}


def document(files):
    return {"type": "llvm.coverage.json.export", "version": "3.1.0",
            "data": [{"files": files, "functions": [], "totals": {}}]}


def project(files):
    report = N["decode"](json.dumps(document(files)))
    return N["project"](report, {f.filename for f in report.files})


def dense_oracle(segments):
    # Deliberately dense small-input reference: search all prior/current entries
    # independently for every line, without the implementation cursor/gap jump.
    result = {}
    for line in range(1, max((s[0] for s in segments), default=0) + 1):
        prior = [s for s in segments if s[0] < line]
        current = [s for s in segments if s[0] == line]
        carry = prior[-1] if prior else None
        starts = [s for s in current if s[3] and s[4] and not s[5]]
        counted_entry = any(s[3] and s[4] for s in current)
        skipped = bool(current) and current[0][3] is False and current[0][4] is True
        live = counted_entry
        if not skipped and (starts or (carry is not None and carry[3])):
            live = True
        if live:
            values = [carry[2] if carry else 0]
            values += [s[2] for s in starts]
            result[line] = max(values)
    return result


class NativeReportControls(unittest.TestCase):
    def test_actual_retained_exports_have_identical_physical_coordinates(self):
        directory = ROOT / "tools/tests/fixtures"
        raw = (directory / "native-overlap.json").read_bytes()
        self.assertEqual(hashlib.sha256(raw).hexdigest(), "37376b9a1e990035b164b0d2470e8dd17770e544c6035a6d863a0cd6cce43986")
        text = (directory / "native-overlap.lcov").read_text()
        self.assertEqual(hashlib.sha256(text.encode()).hexdigest(), "cae4c34e66d749324b297615b43a8dfb7d95e3d959f5085082974e0c50ae221b")
        with self.assertRaisesRegex(ValueError, "Inconsistent LCOV file summary"):
            GATE["parse_lcov"](text)
        expected = {}
        for block in text.split("end_of_record\n"):
            rows = block.split("\n")
            name = next((row[3:] for row in rows if row.startswith("SF:")), None)
            if name:
                expected[name] = {int(row[3:].split(",")[0]): int(row.split(",")[1])
                                  for row in rows if row.startswith("DA:")}
        report = N["decode"](raw.decode())
        actual = N["project"](report, set(expected))
        self.assertEqual(actual, expected)
        self.assertEqual([len(actual[f.filename]) for f in report.files], [195, 43, 272])
        self.assertEqual([f.summary_lines[0] for f in report.files], [208, 43, 280])
        self.assertEqual(report.raw_sha256, hashlib.sha256(raw).hexdigest())
        self.assertEqual(N["PROTOCOL"]["algorithm"], "llvm22-physical-line-v1")

    def test_skipped_gap_wrapped_maximum_and_last_line_have_fixed_oracles(self):
        segments = [[1, 1, 2, True, True, False], [3, 2, 7, True, True, False],
                    [3, 4, 3, True, True, False], [4, 1, 0, False, True, False],
                    [5, 1, 8, True, True, True], [6, 1, 0, False, False, False]]
        self.assertEqual(project([file(segments=segments)])["/source/lib.rs"], {1: 2, 2: 2, 3: 7, 5: 0, 6: 8})
        self.assertEqual(project([file(segments=[])])["/source/lib.rs"], {})
        self.assertEqual(project([file(segments=[[2, 1, 9, True, False, False], [4, 1, 0, False, False, False]])])["/source/lib.rs"], {3: 9, 4: 9})

    def test_small_generated_maps_match_dense_independent_oracle(self):
        rng = random.Random(582041)
        for _ in range(300):
            coordinates = sorted(rng.sample([(line, col) for line in range(1, 9) for col in range(1, 5)], rng.randrange(1, 25)))
            rows = []
            for line, col in coordinates:
                has = bool(rng.randrange(2))
                rows.append([line, col, rng.randrange(6) if has else 0, has, bool(rng.randrange(2)), bool(rng.randrange(2))])
            self.assertEqual(project([file(segments=rows)])["/source/lib.rs"], dense_oracle(rows))

    def test_version_shape_duplicate_keys_and_nonfinite_json_refuse(self):
        good = document([file()])
        variants = []
        for value in ("2.0.0", "3.2.0", 3, None):
            bad = copy.deepcopy(good); bad["version"] = value; variants.append(bad)
        for key, value in (("type", "other"), ("data", []), ("data", [{}]), ("extra", 1)):
            bad = copy.deepcopy(good); bad[key] = value; variants.append(bad)
        for key, value in (("functions", {}), ("totals", []), ("files", {})):
            bad = copy.deepcopy(good); bad["data"][0][key] = value; variants.append(bad)
        for bad in variants:
            with self.subTest(bad=bad), self.assertRaises(ValueError): N["decode"](json.dumps(bad))
        for raw in ('{"type":1,"type":2}', '{"a":1,"\\u0061":2}', 'NaN', 'Infinity', '-Infinity', '1e999', '[', '"open', '"trailing\\\\', '[[[[1]]]', '{"a":-1}', '{"a":18446744073709551616}'):
            with self.subTest(raw=raw), self.assertRaises(ValueError): N["decode"](raw)
        for raw in (b"{}", 1, None):
            with self.assertRaises(ValueError): N["decode"](raw)

    def test_filename_metadata_summary_and_segment_types_refuse(self):
        good = file(segments=[[1, 1, 0, True, True, False]])
        changes = [("filename", ""), ("filename", None), ("filename", "x" * 4097),
                   ("filename", "a\0b"), ("branches", {}), ("expansions", None),
                   ("mcdc_records", False), ("segments", {}), ("summary", {})]
        for key, value in changes:
            bad = copy.deepcopy(good); bad[key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError): N["decode"](json.dumps(document([bad])))
        with self.assertRaises(ValueError): N["decode"](json.dumps(document([good, good])))
        for row in ([1, 1, 0, True, True], [0, 1, 0, True, True, False],
                    [1, 0, 0, True, True, False], [True, 1, 0, True, True, False],
                    [1, 1, True, True, True, False], [1, 1, -1, True, True, False],
                    [1, 1, 2**63, True, True, False], [1, 1, 1, False, True, False],
                    [1, 1, 0, 1, True, False], [1, 1, 0, True, True, 0],
                    [N["MAX_LINE"] + 1, 1, 0, True, True, False],
                    [1, N["MAX_COLUMN"] + 1, 0, True, True, False]):
            with self.subTest(row=row), self.assertRaises(ValueError): project([file(segments=[row])])
        for rows in ([[1, 2, 0, True, True, False], [1, 1, 0, True, True, False]],
                     [[1, 1, 0, True, True, False], [1, 1, 0, True, True, False]]):
            with self.assertRaises(ValueError): project([file(segments=rows)])
        for summary in ({}, {"count": 1, "covered": 2, "percent": 0},
                        {"count": True, "covered": 0, "percent": 0},
                        {"count": 1, "covered": 0, "percent": True},
                        {"count": 1, "covered": 0, "percent": 101}):
            bad = copy.deepcopy(good); bad["summary"]["lines"] = summary
            with self.assertRaises(ValueError): project([bad])

    def test_predecode_and_whole_projection_bounds_and_selection(self):
        env = N["decode"].__globals__
        with patch.dict(env, MAX_BYTES=1), self.assertRaises(ValueError): N["decode"]("{}")
        with self.assertRaisesRegex(ValueError, "nesting"): N["decode"]("[" * 33 + "]" * 33)
        with patch.dict(env, MAX_FILES=1), self.assertRaises(ValueError): project([file("a"), file("b")])
        rows = [[1, 1, 0, True, True, False], [2, 1, 0, False, False, False]]
        with patch.dict(env, MAX_SEGMENTS=1), self.assertRaises(ValueError): project([file(segments=rows)])
        report = N["decode"](json.dumps(document([file("a", rows), file("b", rows)])))
        self.assertEqual(N["project"](report, {"a"}), {"a": {1: 0, 2: 0}})
        self.assertEqual(N["project"](report, set()), {})
        for selected in ({"unknown"}, ["a"]):
            with self.assertRaises(ValueError): N["project"](report, selected)
        with patch.dict(env, MAX_OUTPUT=3), self.assertRaises(ValueError): N["project"](report, {"a", "b"})
        with patch.dict(env, MAX_STEPS=5), self.assertRaises(ValueError): N["project"](report, {"a", "b"})
        # Millions of uncounted leading lines do not allocate or loop over the gap.
        sparse = N["decode"](json.dumps(document([file("sparse", [[2_000_000, 1, 0, False, False, False]])])))
        with patch.dict(env, MAX_STEPS=2):
            self.assertEqual(N["project"](sparse, {"sparse"}), {"sparse": {}})


if __name__ == "__main__":
    unittest.main()
