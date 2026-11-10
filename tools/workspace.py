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
    "hsearch-object": set(),
    "hsearch-object-probe": set(),
    "hsearch-testkit": set(),
    "xtask": set(),
}
PRODUCTION = {"hsearch-types", "hsearch-observe", "hsearch-engine", "hsearch-object"}
EXTERNAL = {name: ({"lucene-analysis", "lucene-codecs", "lucene-index", "lucene-search", "lucene-store"}
                   if name == "hsearch-engine" else set()) for name in LAYERS}
TEST_FEATURES = {"test-util", "test-support", "asserting"}

OPENRAFT_REV = "8815cdba2826f74e848acef361ad03f93bb1c3f8"
RAFT_REV = "10c6e9db6792b85c81784e44fc278f895d5f0ab0"
RESEARCH_PINS = {
    "openraft": ("0.9.25", "git+https://github.com/databendlabs/openraft?rev=" + OPENRAFT_REV + "#" + OPENRAFT_REV),
    "openraft-macros": ("0.9.25", "git+https://github.com/databendlabs/openraft?rev=" + OPENRAFT_REV + "#" + OPENRAFT_REV),
    "raft": ("0.7.0", "git+https://github.com/tikv/raft-rs?rev=" + RAFT_REV + "#" + RAFT_REV),
    "raft-proto": ("0.7.0", "git+https://github.com/tikv/raft-rs?rev=" + RAFT_REV + "#" + RAFT_REV),
}
RESEARCH_DIRECT = {
    "openraft": (None, {"serde", "storage-v2"}, False),
    "raft": (None, {"protobuf-codec"}, False),
    "serde": ("=1.0.228", {"derive"}, True),
    "serde_json": ("=1.0.145", set(), True),
    "tokio": ("=1.48.0", {"rt-multi-thread", "sync", "time", "macros"}, False),
    "protobuf": ("=2.28.0", set(), True),
    "slog": ("=2.8.2", set(), True),
}
RESEARCH_FORBIDDEN = TEST_FEATURES | {"bench", "bt", "loosen-follower-log-revert", "singlethreaded", "failpoints"}
RESEARCH_FEATURES = {
    "openraft": {"serde", "storage-v2"},
    "openraft-macros": set(),
    "raft": {"bytes", "protobuf-codec"},
    "raft-proto": {"bytes", "protobuf-codec"},
}


class PolicyError(ValueError):
    """A dependency graph violates the declared ownership policy."""


def need(condition, message):
    if not condition:
        raise PolicyError(message)


def validate(metadata, production_tree):
    packages = {p["id"]: p for p in metadata["packages"]}
    member_ids = set(metadata["workspace_members"])
    members = [packages[x] for x in metadata["workspace_members"]]
    need({p["name"] for p in members} == set(LAYERS), "Unknown or missing workspace crate")
    owned = {p["name"]: p for p in members}
    for package in packages.values():
        if package["source"] is None:
            need(package["id"] in member_ids, f"Unowned path package: {package['name']}")
        if package["name"] in owned:
            need(package["id"] in member_ids, f"Unowned namesake package: {package['name']}")
    workspace_root = Path(metadata["workspace_root"])
    for name, package in owned.items():
        relative = "xtask" if name == "xtask" else f"crates/{name.removeprefix('hsearch-')}"
        expected = (workspace_root / relative / "Cargo.toml").resolve()
        need(Path(package["manifest_path"]).resolve() == expected,
             f"Workspace manifest ownership mismatch: {name}")
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
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
            if target in RESEARCH_DIRECT or target in RESEARCH_PINS:
                need(target in RESEARCH_DIRECT and name == "hsearch-testkit" and dev, f"Research dependency outside testkit dev target: {name} -> {target}")
                requirement, features, defaults = RESEARCH_DIRECT[target]
                need(dep.get("rename") is None and dep.get("target") is None and dep.get("optional") is False and
                     set(dep["features"]) == features and dep["uses_default_features"] is defaults,
                     f"Research dependency declaration changed: {target}")
                if target in RESEARCH_PINS:
                    need(dep["source"] == RESEARCH_PINS[target][1].split("#")[0], f"Research declaration pin changed: {target}")
                else:
                    need(dep["req"] == requirement and dep["source"] == "registry+https://github.com/rust-lang/crates.io-index",
                         f"Research registry requirement changed: {target}")
            if target.startswith("lucene-"):
                need(name == "hsearch-engine", f"Only engine owns Lucene dependencies: {name}")
            if target in names:
                need(dep["source"] is None and bool(dep.get("path")) and
                     (Path(dep["path"]) / "Cargo.toml").resolve() == Path(owned[target]["manifest_path"]).resolve(),
                     f"Workspace dependency identity mismatch: {name} -> {target}")
                graph[name].add(target)
                alias = (dep.get("rename") or target).replace("-", "_")
                for resolved in nodes[package["id"]]["deps"]:
                    if resolved["name"] == alias:
                        need(resolved["pkg"] == owned[target]["id"],
                             f"Resolved workspace identity mismatch: {name} -> {target}")
            if not dev:
                need(target in LAYERS[name] | EXTERNAL[name], f"Forbidden layer edge: {name} -> {target}")
            need(not (name in PRODUCTION and target == "hsearch-object-probe"), "Diagnostic checker has a production inbound edge")
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
    need({dep["name"] for dep in owned["hsearch-testkit"]["dependencies"]} == set(RESEARCH_DIRECT),
         "Research direct dependency inventory changed")
    research_packages = [p for p in packages.values() if p["name"] in RESEARCH_PINS]
    need(len(research_packages) == len(RESEARCH_PINS) and
         {p["name"] for p in research_packages} == set(RESEARCH_PINS),
         "Research resolved package inventory changed")
    engines = [p for p in packages.values() if p["name"] == "lucene-store"]
    need(len(engines) == 1, "Exactly one pinned lucene-store must resolve")
    for package in packages.values():
        if package["name"] in RESEARCH_PINS:
            version, source = RESEARCH_PINS[package["name"]]
            need(package["version"] == version and package["source"] == source,
                 f"Incorrect research pin: {package['name']}")
            node = nodes.get(package["id"])
            need(node is not None and not RESEARCH_FORBIDDEN.intersection(node["features"]),
                 f"Research test instrumentation selected: {package['name']}")
            need(set(node["features"]) == RESEARCH_FEATURES[package["name"]],
                 f"Research feature union changed: {package['name']}")
        if package["name"].startswith("lucene-"):
            need(package["source"] == ENGINE_SOURCE, f"Incorrect engine pin: {package['name']}")
            node = nodes.get(package["id"])
            need(node is not None and not TEST_FEATURES.intersection(node["features"]),
                 f"Upstream test instrumentation in native experiment graph: {package['name']}")
        if package["id"] not in metadata["workspace_members"]:
            need(bool(package["license"] or package["license_file"]),
                 f"Missing upstream license declaration: {package['name']}")
    for forbidden in ("hsearch-testkit", *sorted(TEST_FEATURES)):
        need(forbidden not in production_tree, f"Release dependency/feature leak: {forbidden}")
    for forbidden in (*sorted(RESEARCH_DIRECT), "raft-proto", "openraft-macros", "protobuf-build"):
        need(not any(line.split(" v", 1)[0] == forbidden for line in production_tree.splitlines()),
             f"Research package entered production closure: {forbidden}")


def cargo(*args):
    result = subprocess.run(["cargo", *args], cwd=ROOT, check=True, capture_output=True, text=True)
    return result.stdout


def inputs():
    data = json.loads(cargo("metadata", "--locked", "--offline", "--all-features", "--format-version", "1"))
    roots = [part for name in sorted(PRODUCTION) for part in ("--package", name)]
    tree = cargo("tree", "--locked", "--offline", *roots, "--all-features",
                 "--edges", "normal,build", "--prefix", "none", "--format", "{p} {f}")
    return data, tree


def inventory(metadata):
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    checksums = {(p["name"], p["version"], p.get("source")): p.get("checksum") for p in lock["package"]}
    rows = []
    for package in metadata["packages"]:
        need(package["source"] is not None or package["id"] in metadata["workspace_members"],
             f"Cannot inventory an unowned path package: {package['name']}")
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
        if target in members:
            dep.update(source=None, path=str(Path(members[target]["manifest_path"]).parent))
        packages[source]["dependencies"].append(dep)

    def namesake(d, p, source):
        replacement = copy.deepcopy(p["hsearch-types"])
        replacement.update(id="unowned-types-0.2.0", version="0.2.0", source=source,
                           manifest_path=str(Path(d["workspace_root"]) / "unowned-types/Cargo.toml"), license="MIT")
        d["packages"].append(replacement)
        p["hsearch-observe"]["dependencies"][0].update(
            source=source, path=str(Path(replacement["manifest_path"]).parent), req="^0.2")
        for node in d["resolve"]["nodes"]:
            if node["id"] == p["hsearch-observe"]["id"]:
                for dep in node["deps"]:
                    if dep["pkg"] == p["hsearch-types"]["id"]:
                        dep["pkg"] = replacement["id"]

    def resolved_alias(d, p):
        for node in d["resolve"]["nodes"]:
            if node["id"] == p["hsearch-observe"]["id"]:
                node["deps"][0]["pkg"] = p["lucene-util"]["id"]

    case("unknown crate", "Unknown or missing", lambda d, p: p["xtask"].update(name="unowned"))
    case("foundation imports adapter", "Forbidden layer edge", lambda d, p: edge(p, "hsearch-types", "hsearch-engine"))
    case("foundation imports external adapter", "Forbidden layer edge", lambda d, p: edge(p, "hsearch-types", "object_store"))
    case("testkit normal edge", "Forbidden layer edge", lambda d, p: edge(p, "hsearch-engine", "hsearch-testkit"))
    case("object imports engine", "Forbidden layer edge", lambda d, p: edge(p, "hsearch-object", "hsearch-engine"))
    case("object testkit normal edge", "Forbidden layer edge", lambda d, p: edge(p, "hsearch-object", "hsearch-testkit"))
    case("object Lucene import", "Only engine owns", lambda d, p: edge(p, "hsearch-object", "lucene-store"))
    case("dev cycle", "dependency cycle", lambda d, p: edge(p, "hsearch-testkit", "hsearch-engine", "dev"))
    case("Lucene outside boundary", "Only engine owns", lambda d, p: edge(p, "hsearch-types", "lucene-store"))
    case("wrong pin", "Incorrect engine pin", lambda d, p: p["lucene-store"].update(source="git+unreviewed"))
    case("upstream test feature", "Test feature in production",
         lambda d, p: next(x for x in p["hsearch-engine"]["dependencies"] if x["name"] == "lucene-store").update(features=["test-util"]))
    case("declared production asserting feature", "Test feature in production",
         lambda d, p: next(x for x in p["hsearch-engine"]["dependencies"] if x["name"] == "lucene-store").update(features=["asserting"]))
    case("transitive asserting feature", "Release dependency/feature leak", lambda d, p: None,
         altered_tree=tree + "\nlucene-search asserting\n")
    def resolved_test_feature(d, packages, name, feature):
        node = next(n for n in d["resolve"]["nodes"] if n["id"] == packages[name]["id"])
        node["features"].append(feature)

    case("resolved dev search asserting", "Upstream test instrumentation",
         lambda d, p: resolved_test_feature(d, p, "lucene-search", "asserting"))
    case("resolved native store test utility", "Upstream test instrumentation",
         lambda d, p: resolved_test_feature(d, p, "lucene-store", "test-util"))
    case("license declaration missing", "Missing upstream license",
         lambda d, p: p["lucene-store"].update(license=None, license_file=None))
    case("testkit is default", "Default members",
         lambda d, p: d["workspace_default_members"].append(p["hsearch-testkit"]["id"]))
    case("publication enabled by default", "publication must be disabled",
         lambda d, p: p["hsearch-types"].update(publish=None))
    case("release transitive feature", "Release dependency/feature leak", lambda d, p: None,
         altered_tree=tree + "\nlucene-store test-util\n")
    case("unowned path namesake", "Unowned path package", lambda d, p: namesake(d, p, None))
    case("registry namesake", "Unowned namesake package",
         lambda d, p: namesake(d, p, "registry+https://github.com/rust-lang/crates.io-index"))
    case("inactive declared path substitution", "Workspace dependency identity mismatch",
         lambda d, p: p["hsearch-observe"]["dependencies"][0].update(path=str(Path(d["workspace_root"]) / "unowned-types")))
    case("resolved alias substitution", "Resolved workspace identity mismatch", resolved_alias)
    case("workspace member path substitution", "Workspace manifest ownership mismatch",
         lambda d, p: p["hsearch-types"].update(manifest_path=str(Path(d["workspace_root"]) / "unowned-types/Cargo.toml")))
    bad_inventory = copy.deepcopy(data)
    namesake(bad_inventory, {p["name"]: p for p in bad_inventory["packages"]}, None)
    try:
        inventory(bad_inventory)
    except PolicyError as exc:
        need("unowned path package" in str(exc), f"Wrong inventory rejection: {exc}")
    else:
        raise PolicyError("Unowned path mislabeled as workspace in inventory")
    controls.append("unowned path inventory")
    case("research wrong git pin", "Incorrect research pin",
         lambda d, p: p["raft"].update(source="git+unreviewed"))
    case("research outside testkit", "outside testkit dev",
         lambda d, p: edge(p, "hsearch-observe", "openraft", "dev"))
    case("research default features", "declaration changed",
         lambda d, p: next(x for x in p["hsearch-testkit"]["dependencies"] if x["name"] == "raft").update(uses_default_features=True))
    case("research extra feature", "declaration changed",
         lambda d, p: next(x for x in p["hsearch-testkit"]["dependencies"] if x["name"] == "openraft").update(features=["serde", "storage-v2", "bench"]))
    case("research floating registry", "registry requirement changed",
         lambda d, p: next(x for x in p["hsearch-testkit"]["dependencies"] if x["name"] == "serde").update(req="^1.0.228"))
    case("research target conditional", "declaration changed",
         lambda d, p: next(x for x in p["hsearch-testkit"]["dependencies"] if x["name"] == "raft").update(target="cfg(unix)"))
    case("research alias substitution", "declaration changed",
         lambda d, p: next(x for x in p["hsearch-testkit"]["dependencies"] if x["name"] == "raft").update(rename="other_raft"))
    case("research resolved instrumentation", "Research test instrumentation",
         lambda d, p: resolved_test_feature(d, p, "openraft", "loosen-follower-log-revert"))
    case("research transitive production leak", "Research package entered production",
         lambda d, p: None, altered_tree=tree + "\nraft-proto v0.7.0\n")
    case("research normal edge", "outside testkit dev",
         lambda d, p: next(x for x in p["hsearch-testkit"]["dependencies"] if x["name"] == "raft").update(kind=None))
    case("research build edge", "outside testkit dev",
         lambda d, p: next(x for x in p["hsearch-testkit"]["dependencies"] if x["name"] == "raft").update(kind="build"))
    case("research optional substitution", "declaration changed",
         lambda d, p: next(x for x in p["hsearch-testkit"]["dependencies"] if x["name"] == "raft").update(optional=True))
    case("research declaration wrong git source", "declaration pin changed",
         lambda d, p: next(x for x in p["hsearch-testkit"]["dependencies"] if x["name"] == "raft").update(source="git+unreviewed"))
    case("research unexpected feature union", "Research feature union changed",
         lambda d, p: resolved_test_feature(d, p, "openraft", "default"))
    case("research missing required feature", "Research feature union changed",
         lambda d, p: next(n for n in d["resolve"]["nodes"] if n["id"] == p["raft"]["id"]).update(features=["protobuf-codec"]))
    case("research macros direct outside boundary", "outside testkit dev",
         lambda d, p: edge(p, "hsearch-observe", "openraft-macros", "dev"))
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
