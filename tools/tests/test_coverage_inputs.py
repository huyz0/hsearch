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
