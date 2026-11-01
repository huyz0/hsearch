"""Counterexamples for impact ownership, transitive selection and final results."""
import copy
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

CI = runpy.run_path(str(Path(__file__).resolve().parents[1] / "ci.py"))
HEAD, BASE = "a" * 40, "b" * 40
OWNERSHIP = {"schema_version": 1, "rules": [
    {"path": "docs/", "impact": "docs"}, {"path": "tools/", "impact": "full"}]}
WORKSPACE = {
    "hsearch-types": {"path": "crates/types/", "dependencies": []},
    "hsearch-observe": {"path": "crates/observe/", "dependencies": ["hsearch-types"]},
    "hsearch-engine": {"path": "crates/engine/", "dependencies": ["hsearch-observe", "hsearch-testkit"]},
    "hsearch-testkit": {"path": "crates/testkit/", "dependencies": []}}


def choose(raw, fallback=None):
    return CI["select"](CI["changed_paths"](raw), WORKSPACE, OWNERSHIP,
                        base=BASE, head=HEAD, fallback=fallback)


class SelectionControls(unittest.TestCase):
    def test_docs_only_skips_native_and_sandbox(self):
        value = choose(b"M\0docs/plan/PROGRESS.md\0")
        self.assertEqual(value["selected_jobs"], ["docs"])
        self.assertEqual(value["packages"], [])
        self.assertEqual(value["coverage_packages"], [])
        self.assertFalse(value["full"])

    def test_transitive_consumers_selected_not_unrelated_packages(self):
        value = choose(b"M\0crates/types/src/lib.rs\0")
        self.assertEqual(value["packages"], ["hsearch-engine", "hsearch-observe", "hsearch-types"])
        self.assertEqual(value["selected_jobs"], ["docs", "linux-x64", "linux-arm64"])
        self.assertEqual(value["profiles"], ["dev", "release"])
        self.assertEqual(value["features"], "all")

    def test_dev_dependency_and_coverage_forward_closure(self):
        value = choose(b"M\0crates/testkit/src/lib.rs\0")
        self.assertEqual(value["packages"], ["hsearch-engine", "hsearch-testkit"])
        self.assertEqual(value["coverage_packages"], ["hsearch-engine", "hsearch-observe", "hsearch-types"])

    def test_rename_uses_both_paths_and_deletion_uses_owner(self):
        value = choose(b"R100\0crates/types/src/old.rs\0crates/testkit/src/new.rs\0")
        self.assertEqual(value["packages"], sorted(WORKSPACE))
        value = choose(b"D\0crates/observe/src/old.rs\0")
        self.assertEqual(value["packages"], ["hsearch-engine", "hsearch-observe"])

    def test_unknown_deleted_owner_manifest_and_shared_paths_run_full(self):
        for raw in (b"M\0new/unknown\0", b"D\0crates/removed/src/lib.rs\0",
                    b"M\0crates/types/Cargo.toml\0", b"M\0tools/ci.py\0"):
            with self.subTest(raw=raw):
                value = choose(raw)
                self.assertTrue(value["full"])
                self.assertEqual(value["selected_jobs"], list(CI["JOBS"]))

    def test_fallback_overrides_docs_optimization(self):
        value = choose(b"M\0docs/plan/PROGRESS.md\0", "incomplete base")
        self.assertTrue(value["full"])
        self.assertEqual(value["packages"], sorted(WORKSPACE))

    def test_truncated_empty_or_conflicted_diff_refused(self):
        for raw in (b"", b"M\0file", b"R100\0old\0", b"U\0file\0", b"M\0../bad\0"):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                CI["changed_paths"](raw)

    def test_graph_uses_optional_build_dev_target_edges_and_renames(self):
        root = Path(__file__).resolve().parents[2]
        packages = []
        for name, item in WORKSPACE.items():
            packages.append({"id": name + "-id", "name": name, "source": None,
                             "manifest_path": str(root / item["path"] / "Cargo.toml"), "dependencies": []})
        by_name = {p["name"]: p for p in packages}
        for kind, optional, target in (("normal", True, None), ("build", False, "cfg(unix)"),
                                       ("dev", False, None)):
            by_name["hsearch-engine"]["dependencies"].append({
                "name": "hsearch-types", "rename": "aliased", "path": str(root / "crates/types"),
                "source": None, "kind": kind, "optional": optional, "target": target})
        metadata = {"packages": packages, "workspace_members": [p["id"] for p in packages]}
        result = CI["graph"](metadata, root)
        self.assertEqual(result["hsearch-engine"]["dependencies"], ["hsearch-types"])
        metadata["packages"][0]["dependencies"].append({"name": "foreign", "source": None,
                                                      "path": str(root / "foreign")})
        with self.assertRaisesRegex(ValueError, "Unowned"):
            CI["graph"](metadata, root)

    def test_missing_base_graph_or_truncated_diff_falls_back_full(self):
        root = Path(__file__).resolve().parents[2]

        def outputs(command, _root):
            if command[:3] == ["git", "rev-parse", "HEAD"]:
                return HEAD.encode()
            if command[0] == "cargo":
                raise subprocess.CalledProcessError(1, command)
            raise subprocess.CalledProcessError(128, command)

        with patch.dict(CI["plan"].__globals__, output=outputs):
            value = CI["plan"](BASE, HEAD, root)
        self.assertTrue(value["full"])
        self.assertEqual(value["selected_jobs"], list(CI["JOBS"]))
        self.assertIn("graph unavailable", value["reasons"][0])
        self.assertIn("base/diff unavailable", value["reasons"][0])

    def test_large_docs_diff_cannot_emit_a_partial_actions_plan(self):
        root = Path(__file__).resolve().parents[2]
        raw = b"".join(b"M\0docs/" + str(index).encode() + b"x" * 190 + b".md\0" for index in range(500))

        def outputs(command, _root):
            if command[0] == "cargo":
                return b"{}"
            if command[1] == "rev-parse":
                return HEAD.encode()
            if command[1] == "diff":
                return raw
            return b""

        with patch.dict(CI["plan"].__globals__, output=outputs, graph=lambda _meta, _root: WORKSPACE):
            value = CI["plan"](BASE, HEAD, root)
        self.assertTrue(value["full"])
        self.assertEqual(value["selected_jobs"], list(CI["JOBS"]))
        self.assertEqual(value["changed_records"], 500)
        self.assertEqual(value["changes_digest"], CI["digest"](CI["changed_paths"](raw)))
        self.assertLess(len(CI["canonical"](value).encode()), 64 * 1024)

    def test_real_non_cargo_policy_cannot_omit_behavioral_or_unknown_edges(self):
        policy = json.loads((Path(__file__).resolve().parents[2] / "ci/ownership.json").read_text())
        for path in ("docs/plan/tools/plan.py", "docs/plan/dependencies.json", "docs/plan/tasks.json",
                     "docs/plan/security.md", "docs/plan/replication-copy.md", "docs/plan/operating-profiles.md",
                     "docs/plan/sandbox.md", "docs/plan/impact-ci.md", "docs/new/unmapped.md",
                     "docs/new/generated.rs", "fixtures/generated.bin"):
            value = CI["select"]([{"status": "M", "paths": [path]}], WORKSPACE, policy, base=BASE, head=HEAD)
            with self.subTest(path=path):
                self.assertTrue(value["full"])
        for path in ("docs/plan/PROGRESS.md", "docs/plan/evidence/observed.txt"):
            value = CI["select"]([{"status": "M", "paths": [path]}], WORKSPACE, policy, base=BASE, head=HEAD)
            self.assertEqual(value["selected_jobs"], ["docs"])


class ResultControls(unittest.TestCase):
    def test_current_zero_case_scope_runs_strict_workspace_corpus_in_both_profiles(self):
        workspace = copy.deepcopy(WORKSPACE)
        workspace['hsearch-engine']['dependencies'] = ['hsearch-types', 'hsearch-testkit']
        value = CI['select']([{'status': 'M', 'paths': ['crates/observe/src/lib.rs']}], workspace, OWNERSHIP, base=BASE, head=HEAD)
        self.assertEqual(value['packages'], ['hsearch-observe'])
        commands = []
        with patch.dict(CI['native'].__globals__, output=lambda command: HEAD.encode() if command[0] == 'git' else b'{}',
                        graph=lambda _metadata, _root: workspace, run=lambda command: commands.append(command),
                        qualified_coverage_base=lambda base, head: base):
            CI['native'](value)
        ordinary = [command for command in commands if command[:3] == ['cargo', 'nextest', 'run']]
        self.assertEqual(len(ordinary), 2)
        self.assertTrue(all('--workspace' in command and '--no-tests=fail' in command for command in ordinary))
        self.assertEqual(sum('--release' in command for command in ordinary), 1)

    def test_full_native_refuses_unqualified_new_production_member_before_commands(self):
        workspace = copy.deepcopy(WORKSPACE)
        workspace['hsearch-new-production'] = {'path': 'crates/new-production/', 'dependencies': []}
        value = CI['select']([{'status': 'M', 'paths': ['shared.rs']}], workspace, OWNERSHIP, base=BASE, head=HEAD)
        commands = []
        with patch.dict(CI['native'].__globals__, output=lambda command: HEAD.encode() if command[0] == 'git' else b'{}',
                        graph=lambda _metadata, _root: workspace, run=lambda command: commands.append(command),
                        qualified_coverage_base=lambda base, head: base):
            with self.assertRaisesRegex(ValueError, 'production.*ownership|coverage.*ownership'):
                CI['native'](value)
        self.assertEqual(commands, [])

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        environment = patch.dict(os.environ, CARGO_TARGET_DIR=str(Path(directory.name) / "native-outputs"))
        environment.start()
        self.addCleanup(environment.stop)
        self.value = choose(b"M\0crates/types/src/lib.rs\0")
        self.results = {job: {"result": "success" if job in self.value["selected_jobs"] else "skipped"}
                        for job in CI["JOBS"]}
        self.results["plan"] = {"result": "success"}

    def test_every_selected_result_must_succeed(self):
        CI["gate"](self.value, self.results)
        for job in self.value["selected_jobs"]:
            for result in ("failure", "cancelled", "skipped", None):
                values = copy.deepcopy(self.results)
                if result:
                    values[job]["result"] = result
                else:
                    del values[job]
                with self.subTest(job=job, result=result), self.assertRaisesRegex(ValueError, "Required selected"):
                    CI["gate"](self.value, values)

    def test_full_matrix_requires_both_sandbox_architectures(self):
        value = choose(b"M\0unknown\0")
        with self.assertRaisesRegex(ValueError, "sandbox-x64"):
            CI["gate"](value, self.results)

    def test_docs_skips_allowed_only_with_explicit_plan(self):
        value = choose(b"M\0docs/readme.md\0")
        results = {job: {"result": "skipped"} for job in CI["JOBS"]}
        results.update(plan={"result": "success"}, docs={"result": "success"})
        CI["gate"](value, results)
        with self.assertRaisesRegex(ValueError, "Required selected"):
            CI["gate"](self.value, results)

    def test_failed_planner_and_missing_selection_cannot_pass(self):
        for result in ("failure", "cancelled", "skipped", None):
            self.results["plan"] = {"result": result}
            with self.assertRaisesRegex(ValueError, "Planner"):
                CI["gate"](self.value, self.results)
        value = copy.deepcopy(self.value)
        value["selected_jobs"].remove("linux-arm64")
        with self.assertRaisesRegex(ValueError, "Missing/ambiguous"):
            CI["validate_plan"](value)

    def test_invalid_package_strings_and_weakened_modes_refused(self):
        for key, value in (("packages", ["--all"]), ("features", "default"),
                           ("profiles", ["dev"]), ("coverage_packages", ["foreign"])):
            plan = copy.deepcopy(self.value)
            plan[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                CI["validate_plan"](plan)

    def test_full_cannot_omit_production_coverage(self):
        value = choose(b"M\0unknown\0")
        value["coverage_packages"] = []
        with self.assertRaisesRegex(ValueError, "Incomplete full coverage"):
            CI["validate_plan"](value)

    def test_native_full_preserves_release_doctests_and_coverage(self):
        value = choose(b"M\0unknown\0")
        commands = []
        with patch.dict(CI["native"].__globals__, output=lambda command: HEAD.encode() if command[0] == "git" else b"{}",
                        graph=lambda _metadata, _root: WORKSPACE,
                        run=lambda command: commands.append(command), qualified_coverage_base=lambda base, head: base):
            CI["native"](value)
        tests = [command for command in commands if command[:2] == ["cargo", "test"]]
        self.assertEqual(len(tests), 2)
        self.assertTrue(all("--workspace" in command and "--all-features" in command for command in tests))
        self.assertEqual(sum("--release" in command for command in tests), 1)
        self.assertTrue(all("--doc" in command for command in tests))
        ordinary = [command for command in commands if command[:3] == ["cargo", "nextest", "run"]]
        self.assertEqual(len(ordinary), 2)
        self.assertTrue(all("--workspace" in command and "--all-features" in command and
                            command[-3:] == ["--profile", "hsearch", "--no-tests=fail"] for command in ordinary))
        self.assertEqual(sum("--release" in command for command in ordinary), 1)
        coverage = next(command for command in commands if "tools/coverage.py" in command)
        self.assertTrue(CI["PRODUCTION"].issubset(coverage))
        self.assertIn("run", coverage)  # Collector run requires both dev and release production gates.
        self.assertEqual(coverage[coverage.index("--base") + 1], BASE)

    def test_stale_plan_cannot_launch_native_commands(self):
        commands = []
        with patch.dict(CI["native"].__globals__, output=lambda _command: BASE.encode(),
                        run=lambda command: commands.append(command)):
            with self.assertRaisesRegex(ValueError, "Stale CI plan"):
                CI["native"](self.value)
        self.assertEqual(commands, [])

    def test_zero_coverable_engine_scope_reaches_na_aware_gate(self):
        workspace = copy.deepcopy(WORKSPACE)
        workspace["hsearch-engine"]["dependencies"] = ["hsearch-testkit"]
        value = CI["select"]([{"status": "M", "paths": ["crates/engine/src/lib.rs"]}],
                             workspace, OWNERSHIP, base=BASE, head=HEAD)
        self.assertEqual(value["coverage_packages"], ["hsearch-engine"])
        commands = []
        with patch.dict(CI["native"].__globals__,
                        output=lambda command: HEAD.encode() if command[0] == "git" else b"{}",
                        graph=lambda _metadata, _root: workspace, run=lambda command: commands.append(command),
                        qualified_coverage_base=lambda base, head: base):
            CI["native"](value)
        coverage = next(command for command in commands if "tools/coverage.py" in command)
        self.assertIn("run", coverage)
        self.assertEqual(commands[-1][-2:], ["--package", "hsearch-engine"])
        self.assertIn("tools/coverage.py", commands[-1])

    def test_missing_base_never_becomes_all_source_percentage_fallback(self):
        value = choose(b"M\0unknown\0")
        value["base"] = None
        commands = []
        with patch.dict(CI["native"].__globals__, output=lambda command: HEAD.encode() if command[0] == "git" else b"{}",
                        graph=lambda _metadata, _root: WORKSPACE, run=lambda command: commands.append(command)):
            with self.assertRaisesRegex(ValueError, "qualified ancestor"):
                CI["native"](value)
        self.assertEqual(commands, [])

    def test_stale_dependency_graph_cannot_launch_native_commands(self):
        commands = []
        with patch.dict(CI["native"].__globals__,
                        output=lambda command: HEAD.encode() if command[0] == "git" else b"{}",
                        graph=lambda _metadata, _root: {}, run=lambda command: commands.append(command)):
            with self.assertRaisesRegex(ValueError, "Stale dependency graph"):
                CI["native"](self.value)
        self.assertEqual(commands, [])


@unittest.skipUnless(os.name == "posix" and shutil.which("cargo") and shutil.which("git"),
                     "Real Git/Cargo metadata controls run in the pinned Linux container")
class RealHistoryControls(unittest.TestCase):
    def test_actual_docs_transitive_rename_delete_and_missing_base(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def command(*args):
                return subprocess.check_output(args, cwd=root, stderr=subprocess.PIPE, timeout=20).decode().strip()

            def commit(message):
                command("git", "add", ".")
                command("git", "commit", "-qm", message)
                return command("git", "rev-parse", "HEAD")

            (root / "ci").mkdir()
            (root / "ci/ownership.json").write_text(json.dumps(OWNERSHIP))
            (root / "Cargo.toml").write_text('[workspace]\nresolver="2"\nmembers=["crates/types","crates/observe"]\n')
            for name in ("types", "observe"):
                (root / f"crates/{name}/src").mkdir(parents=True)
                manifest = f'[package]\nname="hsearch-{name}"\nversion="0.1.0"\nedition="2024"\n'
                if name == "observe":
                    manifest += '[target.\'cfg(unix)\'.build-dependencies]\nhsearch-types={path="../types",optional=true}\n'
                (root / f"crates/{name}/Cargo.toml").write_text(manifest)
                (root / f"crates/{name}/src/lib.rs").write_text("// metadata fixture\n")
            command("git", "init", "-q")
            command("git", "config", "user.name", "CI fixture")
            command("git", "config", "user.email", "fixture@example.invalid")
            command("cargo", "generate-lockfile", "--offline")
            base = commit("initial")
            (root / "docs").mkdir()
            (root / "docs/new.md").write_text("documentation\n")
            docs_head = commit("docs")
            value = CI["plan"](base, docs_head, root)
            self.assertEqual(value["selected_jobs"], ["docs"])
            self.assertNotEqual(value["graph_digest"], CI["digest"]({}))
            (root / "crates/types/src/lib.rs").write_text("// changed fixture\n")
            changed_head = commit("types")
            value = CI["plan"](docs_head, changed_head, root)
            self.assertEqual(value["packages"], ["hsearch-observe", "hsearch-types"])
            command("git", "mv", "crates/types/src/lib.rs", "crates/types/src/renamed.rs")
            # Keep a target so Cargo metadata can still inspect the package.
            (root / "crates/types/src/lib.rs").write_text("// replacement\n")
            rename_head = commit("rename")
            value = CI["plan"](changed_head, rename_head, root)
            self.assertFalse(value["full"])
            self.assertIn("hsearch-observe", value["packages"])
            (root / "crates/types/src/renamed.rs").unlink()
            delete_head = commit("delete")
            self.assertIn("hsearch-observe", CI["plan"](rename_head, delete_head, root)["packages"])
            self.assertTrue(CI["plan"]("0" * 40, delete_head, root)["full"])
            self.assertTrue(CI["plan"]("f" * 40, delete_head, root)["full"])


if __name__ == "__main__":
    unittest.main()


class ActualCoverageBaseControls(unittest.TestCase):
    def test_exact_ancestor_and_root_policy_refuses_unknown_nonroot_bases(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.PIPE, timeout=20).decode().strip()
            git("init", "-q"); git("config", "user.name", "CI base fixture")
            git("config", "user.email", "fixture@example.invalid")
            (root / "original").write_bytes(b"original")
            git("add", "."); git("commit", "-qm", "root")
            initial = git("rev-parse", "HEAD")
            with patch.dict(CI["qualified_coverage_base"].__globals__, ROOT=root):
                for marker in ["", "0" * 40, "parentless-root"]:
                    self.assertEqual(CI["qualified_coverage_base"](marker, initial), "parentless-root")
                git("commit", "--allow-empty", "-qm", "next"); head = git("rev-parse", "HEAD")
                self.assertEqual(CI["qualified_coverage_base"](initial, head), initial)
                for marker in ["", "0" * 40, "parentless-root", "all-production"]:
                    with self.subTest(base=marker), self.assertRaises(ValueError):
                        CI["qualified_coverage_base"](marker, head)
                tree = git("rev-parse", "HEAD^{tree}")
                unrelated = git("commit-tree", tree, "-m", "unrelated root")
                with self.assertRaises(ValueError): CI["qualified_coverage_base"](unrelated, head)
