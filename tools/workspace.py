#!/usr/bin/env python3
"""Check bootstrap dependency ownership and emit a source/license inventory."""
from __future__ import annotations

import argparse
import copy
import json
from pathlib import Path
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
ENGINE_REV = "46e280bc2a208544fb36ecd95932e89cd8bc5618"
ENGINE_SOURCE = f"git+https://github.com/huyz0/lucene-rust?rev={ENGINE_REV}#{ENGINE_REV}"
LAYERS = {
    "hsearch-types": set(),
    "hsearch-observe": {"hsearch-types"},
    "hsearch-engine": set(),
    "hsearch-testkit": set(),
    "xtask": set(),
}
PRODUCTION = {"hsearch-types", "hsearch-observe", "hsearch-engine"}
EXTERNAL = {name: ({"lucene-store"} if name == "hsearch-engine" else set()) for name in LAYERS}
TEST_FEATURES = {"test-util", "test-support"}


class PolicyError(ValueError):
    """A dependency graph violates the declared ownership policy."""


def need(condition, message):
    if not condition:
        raise PolicyError(message)


def validate(metadata, production_tree):
    packages = {p["id"]: p for p in metadata["packages"]}
    members = [packages[x] for x in metadata["workspace_members"]]
    need({p["name"] for p in members} == set(LAYERS), "Unknown or missing workspace crate")
    defaults = {packages[x]["name"] for x in metadata["workspace_default_members"]}
    need(defaults == PRODUCTION, "Default members must be exactly the production crates")
    names = set(LAYERS)
    graph = {name: set() for name in names}
    for package in members:
        name = package["name"]
        need(package["publish"] == [], f"Package publication must be disabled: {name}")
        for dep in package["dependencies"]:
            target = dep["name"]
            dev = dep["kind"] == "dev"
            if target.startswith("lucene-"):
                need(name == "hsearch-engine", f"Only engine owns Lucene dependencies: {name}")
            if target in names:
                graph[name].add(target)
            if not dev:
                need(target in LAYERS[name] | EXTERNAL[name], f"Forbidden layer edge: {name} -> {target}")
            if name in PRODUCTION and not dev:
                need(target != "hsearch-testkit", f"Testkit leaked into production: {name}")
                need(not TEST_FEATURES.intersection(dep["features"]), f"Test feature in production: {name}")
    visiting, visited = set(), set()

    def walk(name):
        need(name not in visiting, f"Workspace dependency cycle at {name}")
        if name in visited:
            return
        visiting.add(name)
        for target in graph[name]:
            walk(target)
        visiting.remove(name)
        visited.add(name)

    for name in graph:
        walk(name)
    engines = [p for p in packages.values() if p["name"] == "lucene-store"]
    need(len(engines) == 1, "Exactly one pinned lucene-store must resolve")
    for package in packages.values():
        if package["name"].startswith("lucene-"):
            need(package["source"] == ENGINE_SOURCE, f"Incorrect engine pin: {package['name']}")
        if package["id"] not in metadata["workspace_members"]:
            need(bool(package["license"] or package["license_file"]),
                 f"Missing upstream license declaration: {package['name']}")
    for forbidden in ("hsearch-testkit", "test-util", "test-support"):
        need(forbidden not in production_tree, f"Release dependency/feature leak: {forbidden}")


def cargo(*args):
    result = subprocess.run(["cargo", *args], cwd=ROOT, check=True, capture_output=True, text=True)
    return result.stdout


def inputs():
    data = json.loads(cargo("metadata", "--locked", "--offline", "--format-version", "1"))
    tree = cargo("tree", "--locked", "--offline", "--package", "hsearch-engine", "--package",
                 "hsearch-observe", "--edges", "normal,build", "--prefix", "none", "--format", "{p} {f}")
    return data, tree


def inventory(metadata):
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    checksums = {(p["name"], p["version"], p.get("source")): p.get("checksum") for p in lock["package"]}
    rows = []
    for package in metadata["packages"]:
        rows.append({
            "name": package["name"], "version": package["version"], "source": package["source"] or "workspace",
            "license": package["license"] or ("upstream license file" if package["license_file"] else "not selected"),
            "checksum": checksums.get((package["name"], package["version"], package["source"])),
        })
    return {"schema_version": 1, "scope": "Resolved bootstrap dependencies, not a binary distribution notice bundle",
            "engine_revision": ENGINE_REV, "packages": sorted(rows, key=lambda p: (p["name"], p["version"], p["source"]))}


def selftest(data, tree):
    members = {p["name"]: p for p in data["packages"] if p["id"] in data["workspace_members"]}
    controls = []

    def case(label, expected, change, altered_tree=None):
        candidate = copy.deepcopy(data)
        by_name = {p["name"]: p for p in candidate["packages"]}
        change(candidate, by_name)
        try:
            validate(candidate, tree if altered_tree is None else altered_tree)
        except PolicyError as exc:
            need(expected in str(exc), f"Wrong rejection for {label}: {exc}")
        else:
            raise PolicyError(f"Unsafe control accepted: {label}")
        controls.append(label)

    sample_dep = copy.deepcopy(members["hsearch-observe"]["dependencies"][0])

    def edge(packages, source, target, kind=None):
        dep = dict(sample_dep, name=target, kind=kind, features=[])
        packages[source]["dependencies"].append(dep)

    case("unknown crate", "Unknown or missing", lambda d, p: p["xtask"].update(name="unowned"))
    case("foundation imports adapter", "Forbidden layer edge", lambda d, p: edge(p, "hsearch-types", "hsearch-engine"))
    case("foundation imports external adapter", "Forbidden layer edge", lambda d, p: edge(p, "hsearch-types", "object_store"))
    case("testkit normal edge", "Forbidden layer edge", lambda d, p: edge(p, "hsearch-engine", "hsearch-testkit"))
    case("dev cycle", "dependency cycle", lambda d, p: edge(p, "hsearch-testkit", "hsearch-engine", "dev"))
    case("Lucene outside boundary", "Only engine owns", lambda d, p: edge(p, "hsearch-types", "lucene-store"))
    case("wrong pin", "Incorrect engine pin", lambda d, p: p["lucene-store"].update(source="git+unreviewed"))
    case("upstream test feature", "Test feature in production",
         lambda d, p: p["hsearch-engine"]["dependencies"][0].update(features=["test-util"]))
    case("license declaration missing", "Missing upstream license",
         lambda d, p: p["lucene-store"].update(license=None, license_file=None))
    case("testkit is default", "Default members",
         lambda d, p: d["workspace_default_members"].append(p["hsearch-testkit"]["id"]))
    case("publication enabled by default", "publication must be disabled",
         lambda d, p: p["hsearch-types"].update(publish=None))
    case("release transitive feature", "Release dependency/feature leak", lambda d, p: None,
         altered_tree=tree + "\nlucene-store test-util\n")
    print(f"PASS: {len(controls)} dependency-policy negative controls: {', '.join(controls)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "inventory", "selftest"))
    args = parser.parse_args()
    try:
        data, tree = inputs()
        validate(data, tree)
        if args.command == "inventory":
            path = ROOT / "docs/plan/dependencies.json"
            path.write_text(json.dumps(inventory(data), indent=2) + "\n", encoding="utf-8", newline="\n")
            print(f"Wrote {path.relative_to(ROOT)}")
        elif args.command == "selftest":
            selftest(data, tree)
        else:
            expected = json.dumps(inventory(data), indent=2) + "\n"
            need((ROOT / "docs/plan/dependencies.json").read_text(encoding="utf-8") == expected,
                 "Dependency inventory is stale; run tools/workspace.py inventory")
            print("PASS: dependency ownership, all-edge cycle check, exact engine pin, release feature boundary, license inventory")
        return 0
    except (PolicyError, OSError, KeyError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError):
            print(error.stderr, file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
