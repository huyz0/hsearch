#!/usr/bin/env python3
"""Check bootstrap dependency ownership and emit a source/license inventory."""
from __future__ import annotations

import argparse
import copy
import json
import re
from pathlib import Path
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
ENGINE_REV = "8c010db716b405f40a06f5d5573f1bd0270bfabf"
ENGINE_SOURCE = f"git+https://github.com/huyz0/lucene-rust?rev={ENGINE_REV}#{ENGINE_REV}"
LAYERS = {
    "hsearch-types": set(),
    "hsearch-observe": {"hsearch-types"},
    "hsearch-engine": set(),
    "hsearch-object": set(),
    "hsearch-s3": {"hsearch-object"},
    "hsearch-index": {"hsearch-object", "hsearch-engine"},
    "hsearch-node": {"hsearch-object", "hsearch-engine", "hsearch-index", "hsearch-s3"},
    "hsearch-module-abi": set(),
    "hsearch-module-native": {"hsearch-module-abi"},
    "hsearch-testkit": set(),
    "xtask": set(),
}
PRODUCTION = {"hsearch-types", "hsearch-observe", "hsearch-engine", "hsearch-object", "hsearch-s3", "hsearch-index", "hsearch-node",
              "hsearch-module-abi", "hsearch-module-native"}
# D18: the one crate whose manifest may relax the workspace `unsafe_code = "forbid"`. Inside it the
# `unsafe_code = "deny"` lint leaves exactly one `#[allow]`, on `mod sys` (the compiler enforces that).
UNSAFE_CRATES = {"hsearch-module-native"}
EXTERNAL = {name: set() for name in LAYERS}
EXTERNAL["hsearch-engine"] = {"lucene-aggs", "lucene-analysis", "lucene-codecs", "lucene-index", "lucene-search", "lucene-store"}
# Signing and a blocking HTTP client, exact-pinned in crates/s3/Cargo.toml. No TLS stack yet.
EXTERNAL["hsearch-s3"] = {"hmac", "md-5", "sha2", "ureq"}
# JSON bodies, a blocking HTTP/1.1 server (plain HTTP, no TLS) and a safe arena for query lowering, exact-pinned in crates/node/Cargo.toml.
EXTERNAL["hsearch-node"] = {"serde_json", "tiny_http", "typed-arena"}
# Extension module contract (JSON descriptor) and the native loader: dynamic library open, SHA-256 pin and a sealed memfd copy.
EXTERNAL["hsearch-module-abi"] = {"serde_json"}
EXTERNAL["hsearch-module-native"] = {"libloading", "rustix", "sha2"}
TEST_FEATURES = {"test-util", "test-support", "asserting"}


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
            node = nodes.get(package["id"])
            need(node is not None and not TEST_FEATURES.intersection(node["features"]),
                 f"Upstream test instrumentation in native experiment graph: {package['name']}")
        if package["id"] not in metadata["workspace_members"]:
            need(bool(package["license"] or package["license_file"]),
                 f"Missing upstream license declaration: {package['name']}")
    for forbidden in ("hsearch-testkit", *sorted(TEST_FEATURES)):
        need(forbidden not in production_tree, f"Release dependency/feature leak: {forbidden}")


def unsafe_boundary(root, manifests):
    """Every crate inherits the workspace lints (`unsafe_code = "forbid"`) except the declared loader crate,
    which restates the same set with `deny` so that one module can opt in."""
    declared = set(root.get("workspace", {}).get("metadata", {}).get("unsafe", {}).get("crates", []))
    need(declared == UNSAFE_CRATES, f"Workspace [workspace.metadata.unsafe] must list exactly {sorted(UNSAFE_CRATES)}")
    need(root["workspace"]["lints"]["rust"]["unsafe_code"] == "forbid", "Workspace must forbid unsafe_code")
    for name, manifest in manifests.items():
        lints = manifest.get("lints")
        if name not in UNSAFE_CRATES:
            need(lints == {"workspace": True}, f"Only {sorted(UNSAFE_CRATES)} may leave the workspace lints: {name}")
            continue
        need(lints is not None and not lints.get("workspace"), f"The unsafe crate must state its own lints: {name}")
        rust, clippy = lints.get("rust", {}), lints.get("clippy", {})
        need(rust.get("unsafe_code") == "deny", f"The unsafe crate must `deny` unsafe_code so only one module opts in: {name}")
        need(rust.get("missing_docs") == "deny" and clippy.get("all") == "deny",
             f"The unsafe crate must keep the rest of the workspace lint set: {name}")


def unsafe_sources(files):
    """`files` maps a path relative to the unsafe crate's src/ to its text. The `unsafe` keyword may appear
    only in sys.rs, and `allow(unsafe_code)` only once, in lib.rs, on `mod sys`: a manifest `deny` can be
    overridden from inside the crate, so the manifest check alone does not hold the boundary."""
    strip = lambda text: "\n".join(line.split("//")[0] for line in text.splitlines())
    for name, text in files.items():
        code = strip(text)
        if name != "sys.rs":
            need(re.search(r"\bunsafe\b", code) is None, f"`unsafe` outside sys.rs: {name}")
        if name != "lib.rs":
            need("allow(unsafe_code)" not in code.replace(" ", ""), f"allow(unsafe_code) outside lib.rs: {name}")
    lib = strip(files["lib.rs"]).replace(" ", "")
    need(lib.count("allow(unsafe_code)") == 1 and "#[allow(unsafe_code)]\nmodsys;" in lib,
         "lib.rs must allow unsafe_code exactly once, on `mod sys`")


def unsafe_source_files():
    return {p.name: p.read_text(encoding="utf-8") for p in (ROOT / "crates/module-native/src").glob("*.rs")}


def manifests_of(metadata):
    owned = {p["name"]: Path(p["manifest_path"]) for p in metadata["packages"] if p["id"] in set(metadata["workspace_members"])}
    return {name: tomllib.loads(path.read_text(encoding="utf-8")) for name, path in owned.items()}


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
    root, manifests = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8")), manifests_of(data)

    def unsafe_case(label, expected, change):
        r, m = copy.deepcopy(root), copy.deepcopy(manifests)
        change(r, m)
        try:
            unsafe_boundary(r, m)
        except PolicyError as exc:
            need(expected in str(exc), f"Wrong rejection for {label}: {exc}")
        else:
            raise PolicyError(f"Unsafe control accepted: {label}")
        controls.append(label)

    unsafe_case("second crate leaves workspace lints", "Only", lambda r, m: m["hsearch-object"].update(lints={"rust": {"unsafe_code": "allow"}}))
    unsafe_case("loader crate inherits workspace lints", "state its own lints", lambda r, m: m["hsearch-module-native"].update(lints={"workspace": True}))
    unsafe_case("loader crate allows unsafe crate-wide", "`deny`", lambda r, m: m["hsearch-module-native"]["lints"]["rust"].update(unsafe_code="allow"))
    unsafe_case("loader crate drops missing_docs", "rest of the workspace lint set", lambda r, m: m["hsearch-module-native"]["lints"]["rust"].pop("missing_docs"))
    unsafe_case("exception list widened", "must list exactly", lambda r, m: r["workspace"]["metadata"]["unsafe"].update(crates=["hsearch-module-native", "hsearch-object"]))
    unsafe_case("workspace stops forbidding", "must forbid", lambda r, m: r["workspace"]["lints"]["rust"].update(unsafe_code="deny"))
    sources = unsafe_source_files()

    def source_case(label, expected, change):
        files = dict(sources)
        change(files)
        try:
            unsafe_sources(files)
        except PolicyError as exc:
            need(expected in str(exc), f"Wrong rejection for {label}: {exc}")
        else:
            raise PolicyError(f"Unsafe control accepted: {label}")
        controls.append(label)

    source_case("unsafe block in module.rs", "outside sys.rs", lambda f: f.update({"module.rs": f["module.rs"] + "\nfn x() { unsafe { } }\n"}))
    source_case("module-wide allow", "outside lib.rs", lambda f: f.update({"module.rs": "#![allow(unsafe_code)]\n" + f["module.rs"]}))
    source_case("second allow in lib.rs", "exactly once", lambda f: f.update({"lib.rs": f["lib.rs"] + "\n#[allow(unsafe_code)]\nmod other;\n"}))
    source_case("allow moved off mod sys", "exactly once", lambda f: f.update({"lib.rs": f["lib.rs"].replace("#[allow(unsafe_code)]\nmod sys;", "mod sys;")}))
    print(f"PASS: {len(controls)} dependency-policy negative controls: {', '.join(controls)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "inventory", "selftest"))
    args = parser.parse_args()
    try:
        data, tree = inputs()
        validate(data, tree)
        unsafe_boundary(tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8")), manifests_of(data))
        unsafe_sources(unsafe_source_files())
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
            print("PASS: dependency ownership, all-edge cycle check, exact engine pin, release feature boundary, single unsafe crate, license inventory")
        return 0
    except (PolicyError, OSError, KeyError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError):
            print(error.stderr, file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
