"""Stale, missing, partial and unexpected coverage inputs must block merging."""
from pathlib import Path
import copy
import runpy
import tempfile
import unittest

INPUT = runpy.run_path(str(Path(__file__).resolve().parents[1] / "coverage_inputs.py"))


def fixture(root):
    for name, content in [("binary", b"original executable fixture"), ("production.o", b"original object fixture"), ("profiles/0/data.profraw", b"original profile fixture")]:
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
    record = INPUT["file_record"]
    context = {"source": {"head": "a" * 40, "tree": "b" * 40, "digest": "c" * 64},
               "toolchain": {"rustc": "original pinned compiler", "llvm": "original pinned LLVM"},
               "features": "all", "profile": "dev", "target": "x86_64-unknown-linux-gnu",
               "flags": ["instrument-coverage"], "generation": "d" * 32,
               "binaries": {"fixture": {"artifact": record(root, "binary"), "test_count": 1}},
               "production_objects": [record(root, "production.o")]}
    shard = {"context_sha256": INPUT["context_digest"](context), "context": copy.deepcopy(context), "binary_id": "fixture",
             "completed": True, "command_exit": 0, "profile_directory": "profiles/0",
             "raw_profiles": [record(root, "profiles/0/data.profraw", single_link=True)]}
    return context, [shard]


class CoverageInputControls(unittest.TestCase):
    def test_complete_immutable_input_inventory_is_returned_without_globbing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            context, shards = fixture(root)
            self.assertEqual(INPUT["validate_shards"](root, context, shards), [root / "profiles/0/data.profraw"])

    def test_source_toolchain_flags_features_profile_target_and_generation_mismatches_refuse(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            context, shards = fixture(root)
            for key in ("source", "toolchain", "flags", "features", "profile", "target", "generation"):
                changed = copy.deepcopy(context)
                changed[key] = "different bound input"
                with self.subTest(key=key), self.assertRaises(ValueError):
                    INPUT["validate_shards"](root, changed, shards)

    def test_missing_or_mutated_binary_object_and_raw_profile_refuse(self):
        for file in ("binary", "production.o", "profiles/0/data.profraw"):
            for mutation in ("remove", "change"):
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    context, shards = fixture(root)
                    path = root / file
                    if mutation == "remove": path.unlink()
                    else: path.write_bytes(b"unexpected changed bytes")
                    with self.subTest(file=file, mutation=mutation), self.assertRaises((ValueError, OSError)):
                        INPUT["validate_shards"](root, context, shards)

    def test_unfinished_failed_missing_and_boolean_receipts_cannot_contribute(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            context, shards = fixture(root)
            for key, value in [("completed", False), ("completed", 1), ("command_exit", 1), ("command_exit", False), ("raw_profiles", [])]:
                changed = copy.deepcopy(shards)
                changed[0][key] = value
                with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                    INPUT["validate_shards"](root, context, changed)
            with self.assertRaises(ValueError):
                INPUT["validate_shards"](root, context, [])
            with self.assertRaises(ValueError):
                INPUT["validate_shards"](root, context, shards + shards)

    def test_unrecorded_stale_profile_is_not_silently_merged(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            context, shards = fixture(root)
            (root / "profiles/0/older.profraw").write_bytes(b"stale profile")
            with self.assertRaisesRegex(ValueError, "Unrecorded"):
                INPUT["validate_shards"](root, context, shards)

    def test_zero_test_binary_still_requires_its_actual_immutable_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            context, shards = fixture(root)
            context["binaries"]["fixture"]["test_count"] = 0
            (root / "profiles/0/data.profraw").unlink()
            shards[0].update(context=copy.deepcopy(context), context_sha256=INPUT["context_digest"](context), raw_profiles=[])
            self.assertEqual(INPUT["validate_shards"](root, context, shards), [])
            (root / "binary").unlink()
            with self.assertRaises((ValueError, OSError)):
                INPUT["validate_shards"](root, context, shards)

    def test_redirected_traversal_paths_and_bool_sizes_refuse_before_reading(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            context, shards = fixture(root)
            for name in ("../outside", "/outside", "C:/outside", "profiles//data", "profiles/./data"):
                with self.subTest(name=name), self.assertRaises(ValueError):
                    INPUT["file_record"](root, name)
            changed = copy.deepcopy(context["binaries"]["fixture"]["artifact"])
            changed["bytes"] = True
            with self.assertRaises(ValueError):
                INPUT["check_record"](root, changed)


class DiagnosticCoverageInputControls(unittest.TestCase):
    def test_diagnostic_binary_uses_same_actual_source_profile_and_shard_admission(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            context, shards = fixture(root)
            context["binaries"]["hsearch-object-probe"] = context["binaries"].pop("fixture")
            shards[0].update(binary_id="hsearch-object-probe", context=copy.deepcopy(context),
                             context_sha256=INPUT["context_digest"](context))
            self.assertEqual(INPUT["validate_shards"](root, context, shards), [root / "profiles/0/data.profraw"])
            shards[0]["context"]["source"]["head"] = "f"*40
            with self.assertRaises(ValueError):
                INPUT["validate_shards"](root, context, shards)

    def test_diagnostic_policy_does_not_change_production_floor_or_zero_line_rules(self):
        import json
        policy = json.loads((Path(__file__).resolve().parents[2] / "harness/coverage-policy.json").read_bytes())
        self.assertEqual(policy["minimum_percent"], 90)
        self.assertEqual(set(policy["development_packages"]), {"xtask", "hsearch-testkit", "hsearch-object-probe"})
        self.assertTrue(policy["diagnostic_packages"]["hsearch-object-probe"]["separate_from_production"])
        self.assertEqual(policy["diagnostic_packages"]["hsearch-object-probe"]["minimum_percent"], 90)


class DiagnosticCollectorScopeControls(unittest.TestCase):
    def test_exact_diagnostic_scope_stays_separate_and_mixed_or_relaxed_policy_refuses(self):
        import json
        import tomllib
        from unittest.mock import patch
        root = Path(__file__).resolve().parents[2]
        collector = runpy.run_path(str(root/"tools/coverage.py"))
        packages = []
        for manifest in (root/"crates").glob("*/Cargo.toml"):
            name = tomllib.loads(manifest.read_text())["package"]["name"]
            packages.append({"id": name, "name": name, "manifest_path": str(manifest), "targets": [{"kind": ["lib"]}]})
        packages.append({"id": "xtask", "name": "xtask", "manifest_path": str(root/"xtask/Cargo.toml"), "targets": []})
        metadata = {"workspace_members": [p["id"] for p in packages], "packages": packages}
        inventory = collector["package_inventory"]
        _packages, production, _policy, _external = inventory(None, [], metadata=metadata)
        self.assertNotIn("hsearch-object-probe", production)
        self.assertIn("hsearch-engine", production)
        self.assertEqual(inventory(None, ["hsearch-object-probe"], metadata=metadata)[1], ["hsearch-object-probe"])
        for scope in (["hsearch-engine", "hsearch-object-probe"], ["xtask"], ["hsearch-object-probe"]*2):
            with self.subTest(scope=scope), self.assertRaises(ValueError):
                inventory(None, scope, metadata=metadata)
        policy = json.loads((root/"harness/coverage-policy.json").read_bytes())
        for changed in (dict(policy, minimum_percent=89), dict(policy, development_packages=policy["development_packages"]+["hsearch-engine"]),
                        dict(policy, diagnostic_packages={"hsearch-object-probe": {"minimum_percent": 89, "separate_from_production": True}})):
            with patch.dict(inventory.__globals__, load=lambda _path: changed):
                with self.assertRaises(ValueError):
                    inventory(None, [], metadata=metadata)
