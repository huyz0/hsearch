#!/usr/bin/env python3
"""Check current bootstrap production coverage, not future merged/changed-line gates."""
import argparse
import json
from pathlib import Path
import sys

PRODUCTION = ("types", "observe", "engine")


def check(report, production=PRODUCTION):
    if not production or not set(production).issubset(PRODUCTION):
        raise ValueError("Unknown or empty production coverage scope")
    counts = {name: [0, 0] for name in production}
    files = set()
    for unit in report["data"]:
        for entry in unit["files"]:
            filename = entry["filename"].replace("\\", "/")
            for name in production:
                if f"/crates/{name}/src/" not in filename:
                    continue
                if filename in files:
                    raise ValueError(f"Duplicate coverage file: {filename}")
                files.add(filename)
                lines = entry["summary"]["lines"]
                total, covered = lines["count"], lines["covered"]
                if type(total) is not int or type(covered) is not int or not 0 <= covered <= total:
                    raise ValueError(f"Invalid coverage counts: {filename}")
                if total and covered * 100 < total * 90:
                    raise ValueError(f"Below 90% production file coverage: {filename}: {covered}/{total}")
                counts[name][0] += covered
                counts[name][1] += total
    # Types currently owns all executable production Rust lines. Observe is a
    # trait and engine is constants/reexports. M01.07 must handle the growing
    # source/binary matrix and reject missing instrumentation for later crates.
    if "types" in counts and not counts["types"][1]:
        raise ValueError("Missing production types coverage")
    for name, (covered, total) in counts.items():
        if total and covered * 100 < total * 90:
            raise ValueError(f"Below 90% crate coverage: {name}: {covered}/{total}")
    return counts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--package", action="append", choices=[f"hsearch-{name}" for name in PRODUCTION])
    args = parser.parse_args()
    try:
        production = tuple(name.removeprefix("hsearch-") for name in args.package) if args.package else PRODUCTION
        counts = check(json.loads(args.report.read_text(encoding="utf-8")), production)
        for name, (covered, total) in counts.items():
            if total:
                print(f"PASS: hsearch-{name}: {covered}/{total} production lines ({100 * covered / total:.2f}%)")
            else:
                print(f"N/A: hsearch-{name}: no executable production lines in this bootstrap")
        return 0
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
