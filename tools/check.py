#!/usr/bin/env python3
"""Run the complete implemented bootstrap suite, stopping on the first failure.

This is not the M01.06 impact planner or the M01.07 coverage gate. Those tasks
remain incomplete until their negative controls and hosted/native matrix run.
"""
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
CHECKS = [
    [sys.executable, "tools/workspace.py", "check"],
    [sys.executable, "tools/workspace.py", "selftest"],
    ["cargo", "fmt", "--all", "--", "--check"],
    ["cargo", "clippy", "--locked", "--offline", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"],
    ["cargo", "test", "--locked", "--offline", "--workspace", "--all-features"],
    ["cargo", "test", "--locked", "--offline", "--workspace", "--all-features", "--release"],
    ["cargo", "build", "--locked", "--offline", "--release", "--package", "hsearch-engine", "--package", "hsearch-observe"],
    [sys.executable, "docs/plan/tools/plan.py", "check"],
    [sys.executable, "docs/plan/tools/plan.py", "selftest"],
    [sys.executable, "docs/plan/tools/cost_model.py", "--selftest"],
    [sys.executable, "-m", "unittest", "discover", "--start-directory", "tools/tests", "--verbose"],
]


def main():
    for command in CHECKS:
        print("CHECK: " + " ".join(command), flush=True)
        try:
            result = subprocess.run(command, cwd=ROOT, check=False)
        except OSError as error:
            print(f"FAIL: {error}", file=sys.stderr)
            return 1
        if result.returncode:
            print(f"FAIL: exit {result.returncode}", file=sys.stderr)
            return 1
    print("PASS: complete bootstrap suite; remaining milestone/runtime gates are recorded in the ledger")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
