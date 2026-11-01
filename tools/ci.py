#!/usr/bin/env python3
"""Select conservative CI work and reject incomplete selected-job results."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import runpy
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[1]
COVERAGE_SOURCES = runpy.run_path(str(ROOT / "tools/coverage_sources.py"))
COVERAGE = runpy.run_path(str(ROOT / "tools/coverage.py"))
JOBS = ("docs", "linux-x64", "linux-arm64", "sandbox-x64", "sandbox-arm64")
PRODUCTION = {"hsearch-types", "hsearch-observe", "hsearch-engine"}
SHA = re.compile(r"[0-9a-f]{40}")


def need(condition, message):
    if not condition:
        raise ValueError(message)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def digest(value):
    return hashlib.sha256(canonical(value).encode()).hexdigest()


def output(command, root=ROOT):
    return subprocess.check_output(command, cwd=root, stderr=subprocess.PIPE, timeout=90)


def graph(metadata, root):
    """Use every declared edge, including optional/build/dev/target dependencies."""
    root = Path(root).resolve()
    packages = {p["id"]: p for p in metadata["packages"]}
    ids = set(metadata["workspace_members"])
    need(ids and ids == set(packages), "Incomplete no-deps workspace metadata")
    need(len(packages) == len(metadata["packages"]), "Duplicate package identity")
    names, paths, result = {}, {}, {}
    for identity in sorted(ids):
        package = packages[identity]
        name = package["name"]
        need(re.fullmatch(r"[a-z][a-z0-9_-]*", name) is not None, "Unsupported package name")
        need(name not in names and package["source"] is None, "Ambiguous package ownership")
        directory = Path(package["manifest_path"]).resolve().parent
        relative = directory.relative_to(root).as_posix()
        need(relative != "." and directory not in paths, "Ambiguous package directory")
        names[name], paths[directory] = identity, name
        result[name] = {"path": relative + "/", "dependencies": []}
    for name, identity in names.items():
        dependencies = set()
        for dependency in packages[identity]["dependencies"]:
            if dependency.get("path"):
                target = paths.get(Path(dependency["path"]).resolve())
                need(target is not None and dependency["source"] is None,
                     "Unowned path dependency")
                need(dependency["name"] == target, "Mismatched package identity")
                dependencies.add(target)
            elif dependency["name"] in names:
                raise ValueError("Unowned workspace namesake")
        result[name]["dependencies"] = sorted(dependencies)
    visiting, visited = set(), set()

    def visit(name):
        need(name not in visiting, "Cyclic workspace graph")
        if name in visited:
            return
        visiting.add(name)
        for dependency in result[name]["dependencies"]:
            visit(dependency)
        visiting.remove(name)
        visited.add(name)

    for name in result:
        visit(name)
    return result


def changed_paths(raw):
    need(len(raw) <= 1024 * 1024 and raw.endswith(b"\0"), "Missing/truncated changed-file list")
    parts = raw.decode("utf-8", errors="strict").split("\0")[:-1]
    changes, index = [], 0
    while index < len(parts):
        status = parts[index]
        index += 1
        count = 2 if re.fullmatch(r"[RC][0-9]{1,3}", status) else 1
        need(count == 2 or status in {"A", "M", "D", "T"}, "Unsupported change status")
        need(index + count <= len(parts), "Truncated rename/copy record")
        paths = parts[index:index + count]
        for path in paths:
            need(path and "\\" not in path and not path.startswith("/") and
                 ".." not in PurePosixPath(path).parts, "Unsupported change path")
        changes.append({"status": status, "paths": paths})
        index += count
    need(changes, "Empty changes require full qualification")
    return changes


def closure(seeds, workspace, reverse=False):
    edges = {name: set(item["dependencies"]) for name, item in workspace.items()}
    if reverse:
        edges = {name: {consumer for consumer, deps in edges.items() if name in deps}
                 for name in workspace}
    selected, pending = set(seeds), list(seeds)
    while pending:
        for name in edges[pending.pop()] - selected:
            selected.add(name)
            pending.append(name)
    return selected


def select(changes, workspace, ownership, *, base, head, fallback=None):
    need(SHA.fullmatch(head) is not None, "Invalid checked head")
    need(ownership["schema_version"] == 1, "Unknown ownership schema")
    rules = ownership["rules"]
    need(rules and all(r["path"] and r["impact"] in {"docs", "full"} for r in rules),
         "Invalid non-Cargo ownership map")
    need(all("suffix" not in rule or isinstance(rule["suffix"], str) and
             re.fullmatch(r"\.[a-z]+", rule["suffix"]) for rule in rules), "Invalid ownership suffix")
    reasons, seeds, full = [], set(), bool(fallback)
    if fallback:
        reasons.append(fallback)
    for change in changes:
        for path in change["paths"]:
            owners = [name for name, item in workspace.items() if path.startswith(item["path"])]
            matches = [rule for rule in rules if (path == rule["path"] or
                       (rule["path"].endswith("/") and path.startswith(rule["path"]))) and
                       ("suffix" not in rule or path.endswith(rule["suffix"]))]
            if PurePosixPath(path).name == "Cargo.toml":
                full = True
                reasons.append(f"manifest changed: {path}")
            elif len(owners) == 1 and not matches:
                seeds.update(owners)
                reasons.append(f"package {owners[0]}: {path}")
            elif matches and all(rule["impact"] == "docs" for rule in matches) and not owners:
                reasons.append(f"documentation: {path}")
            else:
                full = True
                reasons.append(f"shared or unknown impact: {path}")
    affected = set(workspace) if full else closure(seeds, workspace, reverse=True)
    coverage = sorted(PRODUCTION if full else closure(affected, workspace) & PRODUCTION)
    native = full or bool(affected)
    selected = ["docs"] + (["linux-x64", "linux-arm64"] if native else [])
    selected += ["sandbox-x64", "sandbox-arm64"] if full else []
    return {"schema_version": 1, "base": base, "head": head, "full": full,
            "packages": sorted(affected), "coverage_packages": coverage if native else [],
            "features": "all", "profiles": ["dev", "release"], "selected_jobs": selected,
            "graph_digest": digest(workspace), "ownership_digest": digest(ownership),
            "changes": changes, "changes_digest": digest(changes), "reasons": reasons}


def plan(base, head, root=ROOT):
    actual = output(["git", "rev-parse", "HEAD"], root).decode().strip()
    need(actual == head and SHA.fullmatch(head), "Checkout does not match requested head")
    ownership = json.loads((root / "ci/ownership.json").read_text(encoding="utf-8"))
    workspace, changes, failures = {}, [], []
    try:
        metadata = json.loads(output(["cargo", "metadata", "--locked", "--offline", "--no-deps",
                                      "--all-features", "--format-version", "1"], root))
        workspace = graph(metadata, root)
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError):
        failures.append("workspace graph unavailable/incomplete; full matrix")
    try:
        need(SHA.fullmatch(base or "") is not None and base != "0" * 40, "Missing base")
        output(["git", "merge-base", "--is-ancestor", base, head], root)
        changes = changed_paths(output(["git", "diff", "--no-ext-diff", "--no-textconv",
                                       "--name-status", "-z", "--find-renames", base, head, "--"], root))
    except (ValueError, UnicodeError, OSError, subprocess.SubprocessError):
        failures.append("base/diff unavailable, non-ancestor, empty or truncated; full matrix")
    value = select(changes, workspace, ownership, base=base, head=head,
                   fallback="; ".join(failures) if failures else None)
    if len(canonical(value).encode()) > 64 * 1024:
        # Keep Actions outputs bounded; never publish a partial selective plan.
        value = select([], workspace, ownership, base=base, head=head,
                       fallback="changed-file evidence exceeds output budget; full matrix")
        value["changes_digest"], value["changed_records"] = digest(changes), len(changes)
    return value


def validate_plan(value):
    need(value["schema_version"] == 1 and SHA.fullmatch(value["head"]), "Invalid plan identity")
    need(type(value["full"]) is bool, "Invalid full-matrix decision")
    for key in ("packages", "coverage_packages"):
        need(isinstance(value[key], list) and len(value[key]) <= 100 and
             len(set(value[key])) == len(value[key]) and
             all(re.fullmatch(r"[a-z][a-z0-9_-]*", name) for name in value[key]), "Invalid package selection")
    need(set(value["coverage_packages"]).issubset(PRODUCTION), "Unknown coverage ownership")
    if value["full"]:
        need(value["coverage_packages"] == sorted(PRODUCTION), "Incomplete full coverage scope")
    need(value["features"] == "all" and value["profiles"] == ["dev", "release"], "Incomplete test modes")
    native = value["full"] or bool(value["packages"])
    expected = ["docs"] + (["linux-x64", "linux-arm64"] if native else [])
    expected += ["sandbox-x64", "sandbox-arm64"] if value["full"] else []
    need(value["selected_jobs"] == expected, "Missing/ambiguous selected jobs")
    return value


def gate(value, results):
    value = validate_plan(value)
    need(results.get("plan", {}).get("result") == "success", "Planner did not succeed")
    for job in JOBS:
        result = results.get(job, {}).get("result")
        if job in value["selected_jobs"]:
            need(result == "success", f"Required selected job {job}: {result or 'missing'}")
        else:
            need(result in {"success", "skipped"}, f"Unexpected result for unselected job {job}: {result}")


def run(command, root=ROOT, env=None):
    command = COVERAGE["cargo_command"](command)
    print("CHECK: " + " ".join(command), flush=True)
    subprocess.run(command, cwd=root, env=env, check=True)


def docs():
    for command in ([sys.executable, "tools/api_contract.py", "check"],
                    [sys.executable, "docs/plan/tools/plan.py", "check"],
                    [sys.executable, "docs/plan/tools/plan.py", "selftest"],
                    [sys.executable, "docs/plan/tools/cost_model.py", "--selftest"],
                    [sys.executable, "-m", "unittest", "discover", "--start-directory", "tools/tests", "--verbose"]):
        run(command)


def qualified_coverage_base(base, head):
    need(type(base) is str, "Coverage requires an explicit qualified ancestor or proven root")
    candidate = None if base in {"", "0" * 40, "parentless-root"} else base
    need(candidate is None or SHA.fullmatch(candidate), "Invalid coverage base")
    COVERAGE_SOURCES["prove"](ROOT, head=head, base=candidate, sources={})
    return "parentless-root" if candidate is None else candidate


def native(value):
    value = validate_plan(value)
    need(output(["git", "rev-parse", "HEAD"]).decode().strip() == value["head"], "Stale CI plan")
    need("linux-x64" in value["selected_jobs"], "Native work is not selected")
    workspace = graph(json.loads(output(["cargo", "metadata", "--locked", "--offline", "--no-deps",
                                        "--all-features", "--format-version", "1"])), ROOT)
    if not value["full"]:
        need(digest(workspace) == value["graph_digest"], "Stale dependency graph")
        need(set(value["packages"]).issubset(workspace), "Unknown selected package")
        need(value["coverage_packages"] == sorted(closure(value["packages"], workspace) & PRODUCTION),
             "Incomplete selected coverage scope")
    need(set(workspace) - {"xtask", "hsearch-testkit"} == PRODUCTION,
         "Actual production coverage ownership requires a reviewed CI policy update")
    base = qualified_coverage_base(value["base"], value["head"]) if value["coverage_packages"] else None
    coverage = COVERAGE
    coverage["nextest_policy"]()
    tools = coverage["qualified_tools"]()
    environment = coverage["ordinary_environment"](tools)
    def native_run(command): run(coverage["correctness_command"](tools, command), env=environment)
    native_run([sys.executable, "tools/coverage_corpus.py"])
    native_run([sys.executable, "tools/workspace.py", "check"])
    native_run([sys.executable, "tools/workspace.py", "selftest"])
    native_run(["cargo", "fmt", "--all", "--", "--check"])
    package_flags = ["--workspace"] if value["full"] else [part for name in value["packages"] for part in ("--package", name)]
    common = ["--locked", "--offline", "--all-features", *package_flags]
    native_run(["cargo", "clippy", *common, "--all-targets", "--", "-D", "warnings"])
    # Current observe/xtask-only selections have no test cases. Keep the strict
    # no-tests failure by conservatively exercising the known workspace corpus.
    ordinary = common if value["full"] or set(value["packages"]) & {"hsearch-types", "hsearch-engine"} else ["--locked", "--offline", "--all-features", "--workspace"]
    for profile in ([], ["--release"]):
        native_run(coverage["nextest_command"](tools, "run", *ordinary, *profile, "--profile", "hsearch", "--no-tests=fail"))
        native_run(["cargo", "test", *common, *profile, "--doc"])
    production = sorted(PRODUCTION if value["full"] else set(value["packages"]) & PRODUCTION)
    if production:
        native_run(["cargo", "build", "--locked", "--offline", "--release", "--lib",
             *[part for name in production for part in ("--package", name)]])
    if value["coverage_packages"]:
        destination = Path(os.environ["CARGO_TARGET_DIR"]).absolute().parent / ("hsearch-coverage-" + uuid.uuid4().hex)
        native_run([sys.executable, "tools/coverage.py", "run", "--base", base, "--output", str(destination),
             *[part for name in value["coverage_packages"] for part in ("--package", name)]])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    choose = sub.add_parser("plan")
    choose.add_argument("--base", default="")
    choose.add_argument("--head", required=True)
    choose.add_argument("--output", type=Path, required=True)
    sub.add_parser("gate")
    sub.add_parser("native")
    sub.add_parser("docs")
    args = parser.parse_args()
    try:
        if args.command == "plan":
            value = validate_plan(plan(args.base, args.head))
            args.output.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
            print(json.dumps(value, indent=2))
            if os.environ.get("GITHUB_OUTPUT"):
                with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as target:
                    target.write("plan=" + canonical(value) + "\n")
                    target.write("native=" + str("linux-x64" in value["selected_jobs"]).lower() + "\n")
                    target.write("sandbox=" + str("sandbox-x64" in value["selected_jobs"]).lower() + "\n")
        elif args.command == "gate":
            gate(json.loads(os.environ["CI_PLAN"]), json.loads(os.environ["CI_RESULTS"]))
            print("PASS: every selected job succeeded")
        elif args.command == "native":
            native(json.loads(os.environ["CI_PLAN"]))
        else:
            docs()
        return 0
    except (ValueError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
