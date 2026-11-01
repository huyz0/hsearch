#!/usr/bin/env python3
"""Production line accounting components; collection/context authority is separate."""
from pathlib import PurePosixPath
import re


def need(condition, message):
    if not condition:
        raise ValueError(message)


def integer(text):
    need(text.isascii() and text.isdecimal() and len(text) <= 20, "Invalid LCOV integer")
    value = int(text)
    need(value <= 2**64 - 1, "LCOV counter exceeds bound")
    return value


def parse_lcov(text, include=None):
    need(type(text) is str and len(text.encode()) <= 64 * 1024**2, "Coverage report exceeds byte bound")
    result, source, lines, totals, entries, seen = {}, None, {}, {}, 0, set()
    ignored = {"FN", "FNDA", "FNF", "FNH", "BRDA", "BRF", "BRH", "VER"}
    for record in text.split("\n"):
        need(len(record.encode()) <= 16384, "Coverage record exceeds bound")
        if not record:
            continue
        if record == "end_of_record":
            need(source is not None and set(totals) == {"LF", "LH"}, "Incomplete LCOV file summary")
            if include is None or include(source):
                need(totals["LF"] == len(lines) and totals["LH"] == sum(value > 0 for value in lines.values()),
                     "Inconsistent LCOV file summary")
                result[source] = lines
            source, lines, totals = None, {}, {}
            continue
        kind, colon, value = record.partition(":")
        need(colon, "Malformed LCOV record")
        if kind == "TN":
            need(source is None, "Nested LCOV trace header")
        elif kind == "SF":
            need(source is None and value and len(value.encode()) <= 4096 and value not in seen and
                 "\0" not in value, "Duplicate, nested or invalid LCOV source")
            source = value
            seen.add(value)
            need(len(seen) <= 10000, "Coverage source count exceeds bound")
        elif kind == "DA":
            need(source is not None, "LCOV line has no source")
            pair = value.split(",")
            need(len(pair) == 2, "Unknown LCOV line format")
            line, hits = map(integer, pair)
            need(0 < line <= 2_000_000 and line not in lines, "Duplicate or invalid coverage line")
            entries += 1
            need(entries <= 2_000_000, "Coverage line count exceeds bound")
            lines[line] = hits
        elif kind in {"LF", "LH"}:
            need(source is not None and kind not in totals, "Duplicate or misplaced LCOV summary")
            totals[kind] = integer(value)
        else:
            need(source is not None and kind in ignored, "Unknown or misplaced LCOV record")
    need(source is None, "Unterminated LCOV file")
    return result


def relative(path):
    need(type(path) is str and path and len(path.encode()) <= 4096 and "\\" not in path and
         "\0" not in path and ":" not in path and all(ord(char) >= 32 for char in path) and not PurePosixPath(path).is_absolute() and
         all(part not in {"", ".", ".."} for part in path.split("/")), "Invalid coverage source identity")
    return path


def measure(covered, total):
    need(type(covered) is int and type(total) is int and 0 <= covered <= total, "Invalid coverage measurement")
    return {"covered": covered, "coverable": total,
            "percent": round(100 * covered / total, 2) if total else None,
            "status": ("pass" if covered * 100 >= total * 90 else "fail") if total else "not_applicable"}


def evaluate(sources, baseline, runtime, changed, non_executable, production_crates):
    """Input maps must already be bound to actual immutable compiler/profile inputs.

    Baseline comes from empty-profile production objects, not test binaries.
    Runtime maps may contain test-only lines; only production baseline lines count.
    A missing map can be N/A only through a reviewed exact-source-hash assertion.
    """
    need(type(production_crates) is list and 0 < len(production_crates) <= 128 and
         all(type(name) is str and name for name in production_crates) and
         len(set(production_crates)) == len(production_crates), "Unknown or duplicate production crate scope")
    need(type(sources) is dict and 0 < len(sources) <= 10000, "Missing production source inventory")
    for values in (baseline, runtime, changed, non_executable):
        need(type(values) is dict and set(values).issubset(sources), "Unowned coverage source")
    need({value["crate"] for value in sources.values()} == set(production_crates), "Missing production crate source inventory")
    totals = {name: [0, 0] for name in production_crates}
    files, total_changed, ignored_runtime, passed = {}, [0, 0], {}, True
    for path, source in sources.items():
        relative(path)
        need(set(source) == {"crate", "sha256", "line_count"} and re.fullmatch(r"[0-9a-f]{64}", source["sha256"]) and
             type(source["line_count"]) is int and 0 <= source["line_count"] <= 2_000_000, "Unbound production source metadata")
        production, execution = baseline.get(path, {}), runtime.get(path, {})
        for values in (production, execution):
            need(type(values) is dict and all(type(line) is int and 0 < line <= source["line_count"] and
                 type(hits) is int and 0 <= hits <= 2**64 - 1 for line, hits in values.items()), "Invalid source line/counter identity")
        need(all(hits == 0 for hits in production.values()), "Production baseline must have empty-profile counts")
        changed_lines = changed.get(path, [])
        need(type(changed_lines) is list and len(changed_lines) <= source["line_count"] and
             all(type(line) is int and 0 < line <= source["line_count"] for line in changed_lines) and
             len(set(changed_lines)) == len(changed_lines), "Invalid changed-line inventory")
        assertion = non_executable.get(path)
        if production:
            need(assertion is None, "Executable production lines cannot be exempted")
        else:
            need(type(assertion) is dict and set(assertion) == {"sha256", "reason"} and
                 assertion["sha256"] == source["sha256"] and type(assertion["reason"]) is str and
                 16 <= len(assertion["reason"]) <= 2048, "Missing instrumentation is not a zero-coverable source")
        covered = {line for line in production if execution.get(line, 0) > 0}
        delta = set(changed_lines).intersection(production)
        observed = measure(len(covered), len(production))
        changed_result = measure(len(covered.intersection(delta)), len(delta))
        totals[source["crate"]][0] += len(covered)
        totals[source["crate"]][1] += len(production)
        total_changed[0] += changed_result["covered"]
        total_changed[1] += changed_result["coverable"]
        ignored_runtime[path] = len(set(execution) - set(production))
        files[path] = {**observed, "changed": changed_result, "uncovered_lines": sorted(set(production) - covered)[:64],
                       "gap_output_truncated": len(set(production) - covered) > 64,
                       "non_executable_reason": assertion["reason"] if assertion else None}
        passed = passed and observed["status"] != "fail" and changed_result["status"] != "fail"
    crates = {name: measure(*value) for name, value in totals.items()}
    aggregate = measure(sum(v[0] for v in totals.values()), sum(v[1] for v in totals.values()))
    delta = measure(*total_changed)
    passed = passed and aggregate["status"] != "fail" and delta["status"] != "fail" and all(value["status"] != "fail" for value in crates.values())
    return {"passed": passed, "threshold_percent": 90, "aggregate": aggregate, "changed": delta,
            "crates": crates, "files": files, "runtime_lines_outside_production_baseline": ignored_runtime,
            "scope": "Production line accounting only; not source/profile/binary authentication or task completion"}
