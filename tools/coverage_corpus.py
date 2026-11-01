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
    def test_actual_alias_bootstrap_and_target_runner_policy(self):
        cov = runpy.run_path(str(Path(__file__).with_name("coverage.py")))
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            parent = Path(directory)
            crate, helper = parent / "crate", parent / "helper"
            for path in (crate / "src", crate / ".cargo", crate / ".config", helper / "src"):
                path.mkdir(parents=True)
            for path, name in ((crate, "configuration-policy-fixture"), (helper, "configuration-policy-helper")):
                (path / "Cargo.toml").write_text('[package]\nname=' + json.dumps(name) + '\nversion="0.0.0"\nedition="2024"\n[workspace]\n')
            (crate / ".config/nextest.toml").write_bytes((Path(__file__).resolve().parents[1] / ".config/nextest.toml").read_bytes())
            source, config, marker = crate / "src/lib.rs", crate / ".cargo/config.toml", crate / "first"
            source.write_text('pub fn live() -> u32 { 1 }\n#[cfg(test)] mod tests { #[test] fn fail_first() { let p=std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"),"/first")); if !p.exists() { std::fs::write(p,b"seen").unwrap(); panic!("first attempt controlled seed"); } } }\n')
            (helper / "src/main.rs").write_text('fn main() { let a:Vec<String>=std::env::args().skip(1).collect(); let mut c=std::process::Command::new("cargo-nextest"); c.arg("nextest").args(&a); if a.first().is_some_and(|s| s=="run") { c.args(["--retries","1"]); } eprintln!("CONTROLLED_ALIAS_DELEGATE"); std::process::exit(c.status().unwrap().code().unwrap_or(1)); }\n')
            runner = cov["Runner"](parent / "context")
            tools, host, llvm = cov["tool_identity"](runner)
            cov["nextest_policy"]()
            def invoke(args, *, env=None):
                return subprocess.run(args, cwd=crate, env=env or runner.env, text=True,
                                      stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=30)
            arguments = ["run", "--offline", "--profile", "hsearch", "--no-tests=fail"]
            config.write_text('')
            baseline = invoke(cov["nextest_command"](tools, *arguments))
            self.assertEqual(baseline.returncode, 100, baseline.stdout)
            marker.unlink()
            config.write_text('[alias]\nnextest=' + json.dumps(["run", "--quiet", "--offline", "--manifest-path", str(helper / "Cargo.toml"), "--"]) + '\n')
            unsafe_alias = invoke(["cargo", "nextest", *arguments])  # Intentionally reproduce the retired route.
            self.assertEqual(unsafe_alias.returncode, 0, unsafe_alias.stdout)
            self.assertIn("FLAKY", unsafe_alias.stdout)
            marker.unlink()
            fixed_alias = invoke(cov["nextest_command"](tools, *arguments))
            self.assertEqual(fixed_alias.returncode, 100, fixed_alias.stdout)
            self.assertNotIn("CONTROLLED_ALIAS_DELEGATE", fixed_alias.stdout)
            marker.unlink()
            config.write_text('[target.' + host + ']\nrunner=' + json.dumps(str(parent / "missing-runner")) + '\n')
            unbound = dict(runner.env); unbound.pop(tools["test_executor"]["environment_key"])
            unbound_runner = invoke(cov["nextest_command"](tools, *arguments), env=unbound)
            self.assertNotEqual(unbound_runner.returncode, 0)
            self.assertFalse(marker.exists())
            fixed_runner = invoke(cov["nextest_command"](tools, *arguments))
            self.assertEqual(fixed_runner.returncode, 100, fixed_runner.stdout)
            self.assertTrue(marker.exists())
            self.assertIn("first attempt controlled seed", fixed_runner.stdout)
            config.write_text('[env]\nRUSTC_BOOTSTRAP={value="1",force=true}\n')
            source.write_text('#![feature(coverage_attribute)]\npub fn live(x:u32) -> u32 { x+1 }\n#[coverage(off)]\npub fn dead() -> u32 { 41 }\n')
            unqualified = invoke(["cargo", "build", "--lib", "--offline"])
            self.assertEqual(unqualified.returncode, 0, unqualified.stdout)
            def actual_map(label, environment):
                archives = list((Path(environment["CARGO_TARGET_DIR"]) / host).glob("debug/deps/libconfiguration_policy_fixture-*.rlib"))
                self.assertEqual(len(archives), 1)
                objects, output = [], parent / label
                output.mkdir()
                members = subprocess.check_output(["ar", "t", str(archives[0])], text=True, timeout=20).split("\n")
                for index, member in enumerate(item for item in members if item.endswith('.o')):
                    path = output / (str(index) + '.o')
                    with path.open('wb') as target:
                        subprocess.run(["ar", "p", str(archives[0]), member], stdout=target, check=True, timeout=20)
                    objects.append(str(path))
                raw = subprocess.check_output([str(llvm / "llvm-cov"), "export", "--empty-profile", "--format=lcov", objects[0],
                                               *[arg for item in objects[1:] for arg in ("--object", item)]], text=True, timeout=20)
                return sorted(cov["GATE"]["parse_lcov"](raw)[str(source)])
            self.assertEqual(actual_map("suppressed", runner.env), [2])
            protected = cov["Runner"](parent / "protected-context")
            cov["tool_identity"](protected)
            # Production collection and ordinary checks use fresh policy-owned
            # targets: Cargo does not fingerprint this compiler-child variable.
            fixed_build = invoke(cov["cargo_command"](["cargo", "build", "--lib", "--offline"]), env=protected.env)
            self.assertNotEqual(fixed_build.returncode, 0)
            self.assertIn("E0554", fixed_build.stdout)
            fixed_list = invoke(cov["nextest_command"](tools, "list", "--offline"), env=protected.env)
            self.assertNotEqual(fixed_list.returncode, 0)
            self.assertIn("E0554", fixed_list.stdout)
            source.write_text('\npub fn live(x:u32) -> u32 { x+1 }\n\npub fn dead() -> u32 { 41 }\n')
            repaired = invoke(cov["cargo_command"](["cargo", "build", "--lib", "--offline"]), env=protected.env)
            self.assertEqual(repaired.returncode, 0, repaired.stdout)
            self.assertEqual(actual_map("plain", protected.env), [2, 4])
            config.write_text('[build]\ntarget="unqualified-target-does-not-exist"\n')
            unbound = dict(protected.env); unbound.pop("CARGO_BUILD_TARGET")
            unbound_target = invoke(cov["cargo_command"](["cargo", "build", "--lib", "--offline"]), env=unbound)
            self.assertNotEqual(unbound_target.returncode, 0)
            pinned_target = invoke(cov["cargo_command"](["cargo", "build", "--lib", "--offline"]), env=protected.env)
            self.assertEqual(pinned_target.returncode, 0, pinned_target.stdout)
            print(json.dumps({"cargo_effective_policy_control": "pass", "unqualified_alias_retry_exit": 0,
                              "qualified_alias_exit": 100, "target_runner_override": "pinned-transparent",
                              "unqualified_suppressed_lines": [2], "plain_production_lines": [2, 4],
                              "qualified_bootstrap_build": "E0554", "qualified_bootstrap_nextest": "E0554",
                              "fresh_policy_target": True, "native_target_policy": host,
                              "valid_source": "pass"}), flush=True)

    def test_real_cargo_configuration_wrappers_are_explicitly_disabled(self):
        cov = runpy.run_path(str(Path(__file__).with_name("coverage.py")))
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            parent = Path(directory)
            crate = parent / "crate"
            (crate / "src").mkdir(parents=True)
            (crate / ".cargo").mkdir()
            (crate / "Cargo.toml").write_text('[package]\nname="wrapper-policy-fixture"\nversion="0.0.0"\nedition="2024"\n[workspace]\n')
            (crate / "src/lib.rs").write_text('pub fn fixture() -> u32 { 1 }\n')
            missing = parent / "does-not-exist-wrapper"
            command = ["cargo", "check", "--offline"]
            clean = dict(os.environ, CARGO_TARGET_DIR=str(parent / "target"), CARGO_TERM_COLOR="never")
            for name in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"):
                clean.pop(name, None)
            runner = cov["Runner"](parent / "context")
            for key in ("rustc-wrapper", "rustc-workspace-wrapper"):
                (crate / ".cargo/config.toml").write_text('[build]\n' + key + '=' + json.dumps(str(missing)) + '\n')
                seed = subprocess.run(command, cwd=crate, env=clean, text=True, stdout=subprocess.PIPE,
                                      stderr=subprocess.STDOUT, timeout=30)
                self.assertNotEqual(seed.returncode, 0)
                self.assertIn(str(missing), seed.stdout)
                repaired = subprocess.run(command, cwd=crate, env=runner.env, text=True, stdout=subprocess.PIPE,
                                          stderr=subprocess.STDOUT, timeout=30)
                self.assertEqual(repaired.returncode, 0, repaired.stdout)
            print(json.dumps({"cargo_config_wrapper_control": "pass", "configured_wrappers": 2,
                              "unqualified_config": "rejected", "explicit_disabled_policy": "pass"}), flush=True)

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
