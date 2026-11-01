#!/usr/bin/env python3
"""Run the complete implemented bootstrap suite, stopping on the first failure.

Native coverage runs separately with an explicit immutable base and external output.
"""
from pathlib import Path
import subprocess
import runpy
import sys

ROOT = Path(__file__).resolve().parents[1]
COVERAGE = runpy.run_path(str(ROOT / "tools/coverage.py"))
CHECKS = [
    [sys.executable, "tools/api_contract.py", "check"],
    [sys.executable, "tools/workspace.py", "check"],
    [sys.executable, "tools/workspace.py", "selftest"],
    ["cargo", "fmt", "--all", "--", "--check"],
    ["cargo", "clippy", "--locked", "--offline", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings"],
    ["cargo", "nextest", "run", "--locked", "--offline", "--workspace", "--all-features", "--profile", "hsearch", "--no-tests=fail"],
    ["cargo", "test", "--locked", "--offline", "--workspace", "--all-features", "--doc"],
    ["cargo", "nextest", "run", "--locked", "--offline", "--workspace", "--all-features", "--release", "--profile", "hsearch", "--no-tests=fail"],
    ["cargo", "test", "--locked", "--offline", "--workspace", "--all-features", "--release", "--doc"],
    # Retain actual target/profile layout receipts using the same attested test actor and policy.
    ['cargo', 'nextest', 'run', '--locked', '--offline', '--package', 'hsearch-observe', '--all-features', '--profile', 'hsearch', '--no-tests=fail', '-E', 'test(=incarnation_is_lossless_diagnostic_identity_and_never_a_metric_dimension)', '--success-output', 'immediate'],
    ['cargo', 'nextest', 'run', '--locked', '--offline', '--package', 'hsearch-observe', '--all-features', '--release', '--profile', 'hsearch', '--no-tests=fail', '-E', 'test(=incarnation_is_lossless_diagnostic_identity_and_never_a_metric_dimension)', '--success-output', 'immediate'],
    ["cargo", "build", "--locked", "--offline", "--release", "--package", "hsearch-engine", "--package", "hsearch-observe"],
    [sys.executable, "docs/plan/tools/plan.py", "check"],
    [sys.executable, "docs/plan/tools/plan.py", "selftest"],
    [sys.executable, "docs/plan/tools/cost_model.py", "--selftest"],
    [sys.executable, "-m", "unittest", "discover", "--start-directory", "tools/tests", "--verbose"],
    [sys.executable, "tools/coverage_corpus.py"],
]


def main():
    try:
        COVERAGE["nextest_policy"]()
        tools = COVERAGE["qualified_tools"]()
        environment = COVERAGE["ordinary_environment"](tools)
    except (ValueError, OSError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    for command in CHECKS:
        command = COVERAGE["cargo_command"](COVERAGE["correctness_command"](tools, command))
        print("CHECK: " + " ".join(command), flush=True)
        try:
            result = subprocess.run(command, cwd=ROOT, env=environment, check=False)
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
