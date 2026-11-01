"""Actual pinned LLVM archive/production-map controls retained from blocked M01.07 for redesigned M01.12."""
from pathlib import Path
import json
import hashlib
import runpy
import platform
import time
import os
import subprocess
import tempfile
import unittest
import tomllib
from unittest.mock import patch


class ActualOverrideControls(unittest.TestCase):
    def test_actual_pinned_tools_refuse_conflicting_compiler_before_cargo(self):
        cov = runpy.run_path(str(Path(__file__).with_name("coverage.py")))
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            root = Path(directory)
            conflicting = root / "conflicting-rustc"
            conflicting.write_text("Controlled inert alternate compiler; must never execute.\n")
            with patch.dict(os.environ, RUSTC=str(conflicting)):
                runner = cov["Runner"](root / "context")
                with self.assertRaisesRegex(ValueError, "RUSTC"):
                    cov["tool_identity"](runner)
            self.assertEqual(runner.number, 2)  # Real rustc version/sysroot probes; Cargo never starts.
            print(json.dumps({"compiler_override_control": "pass", "pinned_rustc_probes": 2, "conflicting_cargo_compiler": "refused"}), flush=True)


class SharedProductionMapControl(unittest.TestCase):
    def test_real_compiler_shared_module_maps_require_production_ownership(self):
        cov = runpy.run_path(str(Path(__file__).with_name("coverage.py")))
        def run(args, **kwargs):
            return subprocess.check_output(args, text=True, stderr=subprocess.PIPE, timeout=30, **kwargs)
        sysroot = Path(run(["rustc", "--print", "sysroot"]).strip())
        host = next(line.removeprefix("host: ") for line in run(["rustc", "-vV"]).split("\n") if line.startswith("host: "))
        llvm = sysroot / "lib/rustlib" / host / "bin/llvm-cov"
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            parent = Path(directory)
            root = parent / "checkout"
            package, dev = root / "crates/prod", root / "crates/testkit"
            (package / "src").mkdir(parents=True)
            (dev / "src").mkdir(parents=True)
            source = package / "src/lib.rs"
            cases = [("checkout", root / "shared.rs", "Unowned first-party"),
                     ("development", dev / "src/helper.rs", "Unowned first-party"),
                     ("external", parent / "unowned.rs", "Unowned external")]
            for role, helper, error in cases:
                helper.write_text('pub fn uncovered() -> u32 { 2 }\n')
                source.write_text('#[path=' + json.dumps(str(helper)) + '] pub mod shared;\npub fn live() -> u32 { 1 }\n')
                artifact = parent / ("objects-" + role)
                artifact.mkdir()
                archive = artifact / "libfixture.rlib"
                run(["rustc", "--edition=2024", "--crate-type=rlib", "-Cinstrument-coverage", "-Clink-dead-code", str(source), "-o", str(archive)])
                objects = []
                for index, member in enumerate(item for item in run(["ar", "t", str(archive)]).split("\n") if item.endswith('.o')):
                    output = artifact / (str(index) + '.o')
                    with output.open('wb') as target:
                        subprocess.run(["ar", "p", str(archive), member], stdout=target, check=True, timeout=30)
                    objects.append(output)
                self.assertTrue(objects)
                report = run([str(llvm), "export", "--empty-profile", "--format=lcov", str(objects[0]),
                              *[part for path in objects[1:] for part in ("--object", str(path))]])
                maps = cov["GATE"]["parse_lcov"](report)
                self.assertIn(str(helper), maps)
                packages = {"prod": {"manifest_path": str(package / "Cargo.toml")},
                            "hsearch-testkit": {"manifest_path": str(dev / "Cargo.toml")}}
                tracked = {"crates/prod/src/lib.rs"}
                if helper.is_relative_to(root): tracked.add(helper.relative_to(root).as_posix())
                with patch.dict(cov["normalize_report"].__globals__, ROOT=root), self.assertRaisesRegex(ValueError, error):
                    cov["normalize_report"](report, {}, packages, ["prod"], tracked)
                print(json.dumps({"shared_production_map_control": "pass", "role": role,
                                  "real_compiler_files": len(maps), "unowned_shared_source": "refused"}), flush=True)


class CompilerPolicyControls(unittest.TestCase):
    def test_real_lint_and_unsafe_seeds_fail_then_original_safe_source_passes(self):
        workspace = tomllib.loads((Path(__file__).resolve().parents[1] / "Cargo.toml").read_text())
        policy = workspace["workspace"]["lints"]["rust"]["unsafe_code"]
        self.assertEqual(policy, "forbid")
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            root = Path(directory)
            (root / "src").mkdir()
            manifest = root / "Cargo.toml"
            manifest.write_text('[package]\nname="hsearch-policy-fixture"\nversion="0.0.0"\nedition="2024"\n'
                                '[workspace]\n[workspace.lints.rust]\nunsafe_code=' + json.dumps(policy) + '\n[lints]\nworkspace=true\n')
            env = dict(os.environ, CARGO_TARGET_DIR=str(root / "target"), CARGO_TERM_COLOR="never")
            source = root / "src/lib.rs"
            command = ["cargo", "clippy", "--manifest-path", str(manifest), "--offline", "--all-targets", "--all-features", "--", "-D", "warnings"]
            for name, content, diagnostic in [
                ("lint", "pub fn original() { let unused_seed = 1; }\n", "unused variable"),
                ("unsafe", "pub fn original() { unsafe {} }\n", "usage of an `unsafe` block")]:
                source.write_text(content)
                failed = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=30)
                self.assertNotEqual(failed.returncode, 0, name)
                self.assertIn(diagnostic, failed.stdout)
                source.write_text("pub fn original(x: u32) -> u32 { x + 1 }\n")
                subprocess.run(command, env=env, check=True, timeout=30)
            print(json.dumps({"compiler_policy_controls": "pass", "lint_seed": "rejected", "unsafe_seed": "rejected", "safe_repair": "pass"}), flush=True)


class LlvmProductionMaps(unittest.TestCase):
    def test_real_uncovered_production_seed_fails_then_passes_after_test_repair(self):
        gate = runpy.run_path(str(Path(__file__).with_name("coverage_gate.py")))
        inputs = runpy.run_path(str(Path(__file__).with_name("coverage_inputs.py")))
        def run(command, **kwargs):
            try:
                return subprocess.check_output(command, text=True, timeout=30, stderr=subprocess.STDOUT, **kwargs)
            except subprocess.CalledProcessError as error:
                print(json.dumps({"llvm_command": command, "exit": error.returncode, "diagnostic": error.output[:4096]}), flush=True)
                raise
        sysroot = Path(run(["rustc", "--print", "sysroot"]).strip())
        host = next(line.removeprefix("host: ") for line in run(["rustc", "-vV"]).split("\n") if line.startswith("host: "))
        tools = sysroot / "lib/rustlib" / host / "bin"
        llvm_cov, llvm_profdata = str(tools / "llvm-cov"), str(tools / "llvm-profdata")
        source = "\n".join([
            "pub fn live(x: u32) -> u32 { x + 1 }",
            "pub fn dead() -> u32 { 41 }",
            "pub fn generic<T: Default>() -> T { T::default() }",
            "#[cfg(test)] mod tests {",
            "    #[test] fn exercises_live() { assert_eq!(super::live(1), 2); }",
            "}", ""])
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            def stage(name, content, opt_level):
                root = Path(directory) / name
                root.mkdir()
                profile_directory = root / "profiles"
                profile_directory.mkdir()
                code, library, binary = root / "fixture.rs", root / "libfixture.rlib", root / "fixture-tests"
                code.write_text(content)
                flags = ["--edition=2024", "--crate-name=coverage_fixture", "-Cinstrument-coverage", "-Clink-dead-code", "-Copt-level=" + str(opt_level)]
                run(["rustc", *flags, "--crate-type=rlib", str(code), "-o", str(library)])
                run(["rustc", *flags, "--test", str(code), "-o", str(binary)])
                env = dict(os.environ, LLVM_PROFILE_FILE=str(profile_directory / "%p-%m.profraw"), DEBUGINFOD_URLS="")
                self.assertIn("1 passed", run([str(binary)], env=env))
                profiles = sorted(profile_directory.glob("*.profraw"))
                self.assertTrue(profiles)
                members = [member for member in run(["ar", "t", str(library)]).split("\n") if member.endswith(".o")]
                self.assertTrue(members)
                objects = []
                for index, member in enumerate(members):
                    target = root / ("production-" + str(index) + ".o")
                    with target.open("wb") as output:
                        subprocess.run(["ar", "p", str(library), member], check=True, timeout=30, stdout=output)
                    objects.append(str(target))
                context = {"fixture_sha256": hashlib.sha256(code.read_bytes()).hexdigest(), "target": host,
                           "compiler": run(["rustc", "--version"]).strip(), "profile": "dev", "flags": flags,
                           "binaries": {"fixture": {"artifact": inputs["file_record"](root, "fixture-tests"), "test_count": 1}},
                           "production_objects": [inputs["file_record"](root, Path(item).name) for item in objects]}
                shard = {"context_sha256": inputs["context_digest"](context), "context": json.loads(json.dumps(context)),
                         "binary_id": "fixture", "completed": True, "command_exit": 0, "profile_directory": "profiles",
                         "raw_profiles": [inputs["file_record"](root, "profiles/" + item.name, single_link=True) for item in profiles]}
                self.assertEqual(inputs["validate_shards"](root, context, [shard]), profiles)
                with self.assertRaises(ValueError):
                    inputs["validate_shards"](root, {**context, "profile": "release"}, [shard])
                stale = profile_directory / "unrecorded.profraw"
                stale.write_bytes(b"controlled stale seed")
                with self.assertRaises(ValueError):
                    inputs["validate_shards"](root, context, [shard])
                self.assertEqual(stale.read_bytes(), b"controlled stale seed")
                stale.unlink()
                preserved = root / "fixture-tests-preserved"
                binary.rename(preserved)
                try:
                    with self.assertRaises((ValueError, OSError)):
                        inputs["validate_shards"](root, context, [shard])
                finally:
                    preserved.rename(binary)
                self.assertEqual(inputs["validate_shards"](root, context, [shard]), profiles)
                merged = root / "merged.profdata"
                run([llvm_profdata, "merge", "--sparse", "--num-threads=2", *map(str, profiles), "-o", str(merged)])
                runtime = gate["parse_lcov"](run([llvm_cov, "export", "--format=lcov", "--num-threads=2", "--instr-profile=" + str(merged), str(binary)]))
                baseline = gate["parse_lcov"](run([llvm_cov, "export", "--format=lcov", "--num-threads=2", "--empty-profile", objects[0],
                                                *[argument for item in objects[1:] for argument in ("--object", item)]]))
                self.assertEqual(set(baseline), {str(code)})
                self.assertEqual(set(runtime), {str(code)})
                self.assertEqual(baseline[str(code)], {1: 0, 2: 0, 3: 0})
                metadata = {"fixture.rs": {"crate": "fixture", "sha256": hashlib.sha256(code.read_bytes()).hexdigest(), "line_count": content.count("\n") + int(bool(content) and not content.endswith("\n"))}}
                report = gate["evaluate"](metadata, {"fixture.rs": baseline[str(code)]}, {"fixture.rs": runtime[str(code)]},
                                          {"fixture.rs": [2]}, {}, ["fixture"])
                return report
            repaired_source = source.replace("assert_eq!(super::live(1), 2);", "assert_eq!(super::live(1), 2); assert_eq!(super::dead(), 41); assert_eq!(super::generic::<u32>(), 0);")
            for profile, opt_level in [("dev", 0), ("release", 3)]:
                rejected = stage("uncovered-" + profile, source, opt_level)
                self.assertFalse(rejected["passed"])
                self.assertEqual(rejected["aggregate"]["coverable"], 3)
                self.assertEqual(rejected["aggregate"]["covered"], 1)
                self.assertEqual(rejected["changed"]["covered"], 0)
                accepted = stage("repaired-tests-" + profile, repaired_source, opt_level)
                self.assertTrue(accepted["passed"])
                self.assertEqual(accepted["aggregate"]["coverable"], 3)
                self.assertEqual(accepted["aggregate"]["covered"], 3)
                self.assertEqual(accepted["runtime_lines_outside_production_baseline"]["fixture.rs"], 1)
                print(json.dumps({"real_coverage_gate": "pass", "profile": profile, "uncovered_seed": rejected["aggregate"], "test_repair": accepted["aggregate"],
                                  "unused_generic_retained": True, "test_only_line_excluded": True,
                                  "stale_context_rejected": True, "unrecorded_profile_rejected": True, "missing_binary_rejected": True}), flush=True)


class NextestDeadlineControl(unittest.TestCase):
    def test_inventory_and_timeout_are_real_and_bounded(self):
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text('[package]\nname="coverage-nextest-fixture"\nversion="0.0.0"\nedition="2024"\n[workspace]\n')
            (root / "src/lib.rs").write_text('#[test] fn fast() { assert_eq!(2+2,4); }\n#[test] fn timeout_seed() { std::thread::sleep(std::time::Duration::from_secs(10)); }\n'
                '#[test] fn retry_seed() { let marker = std::path::PathBuf::from(std::env::var_os("HSEARCH_RETRY_SEED").unwrap()); '
                'if !marker.exists() { std::fs::write(marker, b"controlled initial failure").unwrap(); panic!("controlled initial failure"); } }\n')
            config = root / "nextest.toml"
            config.write_text('[profile.default]\nretries=0\ntest-threads=1\nslow-timeout={period="100ms",terminate-after=1,grace-period="100ms",on-timeout="fail"}\nglobal-timeout="3s"\n')
            listing = root / "listing-profiles"
            listing.mkdir()
            env = dict(os.environ, CARGO_TARGET_DIR=str(root / "target"), CARGO_TERM_COLOR="never",
                       RUSTFLAGS="-C instrument-coverage", LLVM_PROFILE_FILE=str(listing / "%m-%p.profraw"))
            subprocess.run(["cargo", "generate-lockfile", "--offline", "--manifest-path", str(root / "Cargo.toml")], check=True, timeout=30, env=env)
            base = ["cargo", "nextest"]
            common = ["--manifest-path", str(root / "Cargo.toml"), "--locked", "--offline", "--config-file", str(config)]
            listed = subprocess.check_output([*base, "list", *common, "--message-format", "json"], text=True, timeout=30, env=env)
            inventory = json.loads(listed)
            self.assertTrue(list(listing.glob("*.profraw")))
            self.assertFalse(list(root.glob("*.profraw")))
            suites = list(inventory["rust-suites"].values())
            self.assertEqual(len(suites), 1)
            self.assertEqual(set(suites[0]["testcases"]), {"fast", "timeout_seed", "retry_seed"})
            self.assertTrue(Path(suites[0]["binary-path"]).is_file())
            passing = subprocess.run([*base, "run", *common, "-E", "test(=fast)"], text=True, timeout=10, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            self.assertEqual(passing.returncode, 0, passing.stdout)
            self.assertIn("1 passed", passing.stdout)
            started = time.monotonic()
            failed = subprocess.run([*base, "run", *common, "-E", "test(=timeout_seed)"], text=True, timeout=10, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn("TIMEOUT", failed.stdout)
            self.assertLess(time.monotonic() - started, 5)
            marker = root / "retry-seed-receipt"
            overridden = dict(env, NEXTEST_RETRIES="1", HSEARCH_RETRY_SEED=str(marker))
            unsafe_retry = subprocess.run([*base, "run", *common, "-E", "test(=retry_seed)"], env=overridden, text=True,
                                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10)
            self.assertEqual(unsafe_retry.returncode, 0, unsafe_retry.stdout)
            self.assertTrue(marker.is_file())
            marker.unlink()
            fixed = subprocess.run([*base, "run", *common, "--retries", "0", "-E", "test(=retry_seed)"], env=overridden, text=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10)
            self.assertNotEqual(fixed.returncode, 0, fixed.stdout)
            cov = runpy.run_path(str(Path(__file__).with_name("coverage.py")))
            with patch.dict(os.environ, NEXTEST_RETRIES="1"), self.assertRaisesRegex(ValueError, "environment override"):
                cov["nextest_policy"]()
            print(json.dumps({"nextest_inventory": "pass", "tests": 3, "positive_test": "pass", "timeout_seed": "rejected", "exit": failed.returncode,
                              "retry_override_false_success_observed": True, "explicit_no_retry_seed": "rejected", "production_override": "refused"}), flush=True)


if __name__ == "__main__":
    if platform.system() != "Linux":
        raise SystemExit("Compiler coverage/deadline corpus requires qualified Linux")
    parent = Path(os.environ["CARGO_TARGET_DIR"]).absolute()
    if parent.resolve() != parent or parent.is_relative_to(Path(__file__).resolve().parents[1]):
        raise SystemExit("Compiler corpus target must be private and outside source")
    parent.mkdir(parents=True, exist_ok=True)
    unittest.main()
