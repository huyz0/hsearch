#!/usr/bin/env python3
"""The one local/CI check: fmt, clippy, tests, doc tests, plan and policy checks.

Plain `cargo` only. Coverage is `scripts/coverage.sh` (cargo llvm-cov); it is not
part of this command so a developer loop stays fast.
"""
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
CHECKS = [
    [sys.executable, "tools/api_contract.py", "check"],
    [sys.executable, "tools/engine_research.py", "check"],
    [sys.executable, "tools/workspace.py", "check"],
    [sys.executable, "tools/workspace.py", "selftest"],
    ["cargo", "fmt", "--all", "--", "--check"],
    ["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"],
    ["cargo", "test", "--locked", "--workspace", "--all-features"],
    [sys.executable, "docs/plan/tools/plan.py", "check"],
    [sys.executable, "docs/plan/tools/plan.py", "selftest"],
    [sys.executable, "docs/plan/tools/cost_model.py", "--selftest"],
    [sys.executable, "-m", "unittest", "discover", "--start-directory", "tools/tests"],
]


def main():
    for command in CHECKS:
        print("CHECK: " + " ".join(command), flush=True)
        if subprocess.run(command, cwd=ROOT).returncode != 0:
            print("FAIL: " + " ".join(command), file=sys.stderr)
            return 1
    print("PASS: all checks")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
