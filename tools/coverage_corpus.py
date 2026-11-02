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
            shared_build = parent / "shared-build"
            config.write_text('[build]\nbuild-dir=' + json.dumps(str(shared_build)) + '\n[env]\nRUSTC_BOOTSTRAP={value="1",force=true}\n')
            source.write_text('#![feature(coverage_attribute)]\npub fn live(x:u32) -> u32 { x+1 }\n#[coverage(off)]\npub fn dead() -> u32 { 41 }\n')
            unqualified_env = dict(runner.env, CARGO_BUILD_BUILD_DIR=str(shared_build))
            build_args = ["cargo", "build", "--lib", "--offline", "--message-format=json"]
            unqualified = invoke(build_args, env=unqualified_env)
            self.assertEqual(unqualified.returncode, 0, unqualified.stdout)
            def actual_map(label, build):
                archives = []
                for line in build.stdout.split("\n"):
                    try: event = json.loads(line)
                    except ValueError: continue
                    if type(event) is dict and event.get("reason") == "compiler-artifact":
                        archives.extend(Path(path) for path in event["filenames"] if path.endswith(".rlib"))
                self.assertEqual(len(archives), 1)
                self.assertTrue(archives[0].resolve() == archives[0] and archives[0].is_relative_to(parent))
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
                legacy = cov["GATE"]["parse_lcov"](raw)
                native_raw = subprocess.check_output([str(llvm / "llvm-cov"), "export", "--empty-profile", "--format=text", objects[0],
                    *[argument for item in objects[1:] for argument in ("--object", item)]], cwd=crate, env=runner.env, text=True, timeout=30)
                native_report = cov["REPORTS"]["decode"](native_raw)
                self.assertEqual(cov["REPORTS"]["project"](native_report, set(legacy)), legacy)
                return sorted(legacy[str(source)])
            self.assertEqual(actual_map("suppressed", unqualified), [2])
            reused_env = dict(unqualified_env, CARGO_TARGET_DIR=str(parent / "unqualified-new-target"))
            reused = invoke(cov["cargo_command"](build_args), env=reused_env)
            self.assertEqual(reused.returncode, 0, reused.stdout)
            protected = cov["Runner"](parent / "protected-context")
            cov["tool_identity"](protected)
            # Production collection and ordinary checks use fresh policy-owned
            # targets: Cargo does not fingerprint this compiler-child variable.
            fixed_build = invoke(cov["cargo_command"](build_args), env=protected.env)
            self.assertNotEqual(fixed_build.returncode, 0)
            self.assertIn("E0554", fixed_build.stdout)
            fixed_list = invoke(cov["nextest_command"](tools, "list", "--offline"), env=protected.env)
            self.assertNotEqual(fixed_list.returncode, 0)
            self.assertIn("E0554", fixed_list.stdout)
            source.write_text('\npub fn live(x:u32) -> u32 { x+1 }\n\npub fn dead() -> u32 { 41 }\n')
            repaired = invoke(cov["cargo_command"](build_args), env=protected.env)
            self.assertEqual(repaired.returncode, 0, repaired.stdout)
            self.assertEqual(actual_map("plain", repaired), [2, 4])
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
                              "unqualified_shared_cache_exit": reused.returncode, "private_intermediate_cache": "E0554",
                              "valid_source": "pass"}), flush=True)

    def test_actual_clippy_format_aliases_and_forced_lint_arguments(self):
        cov = runpy.run_path(str(Path(__file__).with_name("coverage.py")))
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            parent = Path(directory); crate, helper = parent / "crate", parent / "helper"
            for path in (crate / "src", crate / ".cargo", helper / "src"): path.mkdir(parents=True)
            for path, name in ((crate, "external-tool-fixture"), (helper, "external-tool-delegate")):
                (path / "Cargo.toml").write_text('[package]\nname=' + json.dumps(name) + '\nversion="0.0.0"\nedition="2024"\n[workspace]\n')
            runner = cov["Runner"](parent / "identity")
            tools, _host, _llvm = cov["tool_identity"](runner)
            target = parent / "ordinary-target"; target.mkdir()
            env = cov["ordinary_environment"]({**tools, "ordinary_target": str(target)})
            env.update(CONTROLLED_CARGO=str(Path(tools["cargo_compiler"]).with_name("cargo")), CONTROLLED_POLICY=str(cov["COMPILER_POLICY"]))
            (helper / "src/main.rs").write_text('fn main() { let a:Vec<String>=std::env::args().skip(1).collect(); eprintln!("CONTROLLED_EXTERNAL_ALIAS"); if a.first().is_some_and(|s| s=="fmt") { return; } let s=std::process::Command::new(std::env::var("CONTROLLED_CARGO").unwrap()).args(["--config", &std::env::var("CONTROLLED_POLICY").unwrap(), "check", "--offline", "--all-targets", "--all-features"]).status().unwrap(); std::process::exit(s.code().unwrap_or(1)); }\n')
            source, config = crate / "src/lib.rs", crate / ".cargo/config.toml"
            source.write_text('pub fn original(x: u32) -> u32 {\n    x + 0\n}\n'); config.write_text('')
            def invoke(args):
                return subprocess.run(args, cwd=crate, env=env, stdin=subprocess.DEVNULL, text=True,
                                      stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=30)
            clippy = ["cargo", "clippy", "--offline", "--all-targets", "--all-features", "--", "-D", "warnings"]
            qualified_clippy = cov["correctness_command"](tools, clippy)
            baseline = invoke(qualified_clippy)
            self.assertNotEqual(baseline.returncode, 0)
            self.assertIn("identity_op", baseline.stdout)
            alias = ["run", "--quiet", "--offline", "--manifest-path", str(helper / "Cargo.toml"), "--"]
            config.write_text('[alias]\nclippy=' + json.dumps(alias + ["clippy"]) + '\n')
            unqualified_alias = invoke(cov["cargo_command"](clippy))
            self.assertEqual(unqualified_alias.returncode, 0, unqualified_alias.stdout)
            direct = invoke(qualified_clippy)
            self.assertNotEqual(direct.returncode, 0)
            self.assertIn("identity_op", direct.stdout)
            self.assertNotIn("CONTROLLED_EXTERNAL_ALIAS", direct.stdout)
            config.write_text('[env]\nCLIPPY_ARGS={value="",force=true}\n')
            unqualified_args = invoke([tools["cargo_clippy_executable"], "clippy", *clippy[2:]])
            self.assertEqual(unqualified_args.returncode, 0, unqualified_args.stdout)
            self.assertIn("identity_op", unqualified_args.stdout)
            qualified_args = invoke(qualified_clippy)
            self.assertNotEqual(qualified_args.returncode, 0)
            self.assertIn("identity_op", qualified_args.stdout)
            source.write_text('pub fn original(x:u32)->u32{x+0}\n'); config.write_text('')
            fmt = ["cargo", "fmt", "--all", "--", "--check"]
            qualified_fmt = cov["correctness_command"](tools, fmt)
            bad_format = invoke(qualified_fmt)
            self.assertNotEqual(bad_format.returncode, 0)
            self.assertIn("Diff in", bad_format.stdout)
            config.write_text('[alias]\nfmt=' + json.dumps(alias + ["fmt"]) + '\n')
            alias_format = invoke(cov["cargo_command"](fmt))
            self.assertEqual(alias_format.returncode, 0, alias_format.stdout)
            fixed_format = invoke(qualified_fmt)
            self.assertNotEqual(fixed_format.returncode, 0)
            self.assertIn("Diff in", fixed_format.stdout)
            self.assertNotIn("CONTROLLED_EXTERNAL_ALIAS", fixed_format.stdout)
            config.write_text('')
            source.write_text('pub fn original(x: u32) -> u32 {\n    x\n}\n')
            good_lint, good_format = invoke(qualified_clippy), invoke(qualified_fmt)
            self.assertEqual(good_lint.returncode, 0, good_lint.stdout)
            self.assertEqual(good_format.returncode, 0, good_format.stdout)
            print(json.dumps({"external_rust_tool_policy": "pass", "unqualified_clippy_alias": 0,
                              "qualified_clippy_alias": "identity_op", "unqualified_child_args": 0,
                              "qualified_child_args": "identity_op", "unqualified_fmt_alias": 0,
                              "qualified_fmt_alias": "format_diff", "repaired_lint_format": "pass"}), flush=True)

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
                native_report = run([str(llvm), "export", "--empty-profile", "--format=text", str(objects[0]),
                                     *[part for path in objects[1:] for part in ("--object", str(path))]])
                decoded = cov["REPORTS"]["decode"](native_report)
                self.assertEqual(cov["REPORTS"]["project"](decoded, set(maps)), maps)
                self.assertIn(str(helper), maps)
                packages = {"prod": {"manifest_path": str(package / "Cargo.toml")},
                            "hsearch-testkit": {"manifest_path": str(dev / "Cargo.toml")}}
                tracked = {"crates/prod/src/lib.rs"}
                if helper.is_relative_to(root): tracked.add(helper.relative_to(root).as_posix())
                with patch.dict(cov["normalize_report"].__globals__, ROOT=root), self.assertRaisesRegex(ValueError, error):
                    cov["normalize_report"](native_report, {}, packages, ["prod"], tracked)
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
                native = runpy.run_path(str(Path(__file__).with_name("coverage_reports.py")))
                for oracle, args in [
                    (baseline, [llvm_cov, "export", "--format=text", "--num-threads=2", "--empty-profile", objects[0],
                                *[argument for item in objects[1:] for argument in ("--object", item)]]),
                    (runtime, [llvm_cov, "export", "--format=text", "--num-threads=2", "--instr-profile=" + str(merged), str(binary)]),
                ]:
                    exported = native["decode"](run(args))
                    self.assertEqual(native["project"](exported, set(oracle)), oracle)
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


class NativeProjectionCostControl(unittest.TestCase):
    def test_fresh_native_projection_cost_and_sparse_work_are_bounded(self):
        import sys
        program = r"""
import hashlib,json,resource,runpy,sys,time
m=runpy.run_path(sys.argv[1]);n=int(sys.argv[2])
segments=[[line,1,1,True,True,False] for line in range(1,n+1)]
segments.append([n+1,1,0,False,False,False])
file={"filename":"/fixed/source.rs","segments":segments,"branches":[],"expansions":[],"mcdc_records":[],
      "summary":{"lines":{"count":n+1,"covered":n+1,"percent":100}}}
text=json.dumps({"type":"llvm.coverage.json.export","version":"3.1.0",
                 "data":[{"files":[file],"functions":[],"totals":{}}]})
started=time.monotonic()
report=m["decode"](text);output=m["project"](report,{"/fixed/source.rs"})
assert len(output["/fixed/source.rs"])==n+1 and set(output["/fixed/source.rs"].values())=={1}
print(json.dumps({"segments":n+1,"physical_lines":n+1,"raw_bytes":len(text.encode()),
                 "seconds":time.monotonic()-started,"maximum_rss_kib":resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                 "raw_sha256":report.raw_sha256,"module_sha256":hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest()}))
"""
        reports = []
        module = Path(__file__).with_name("coverage_reports.py")
        for size in (1000, 100000):
            run = subprocess.run([sys.executable, "-c", program, str(module), str(size)], check=True,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=20)
            report = json.loads(run.stdout)
            self.assertEqual(report["physical_lines"], size + 1)
            self.assertLess(report["seconds"], 10)
            self.assertLess(report["maximum_rss_kib"], 512 * 1024)
            self.assertEqual(report["module_sha256"], hashlib.sha256(module.read_bytes()).hexdigest())
            reports.append(report)
        self.assertGreater(reports[1]["raw_bytes"], reports[0]["raw_bytes"])
        print(json.dumps({"native_projection_cost": "pass", "scope": "fresh Linux subprocess per admitted sample; measured peak RSS, not a separate process memory cap or maximum-profile qualification",
                          "samples": reports}), flush=True)


class NativePhysicalOverlapControl(unittest.TestCase):
    def test_real_overlap_and_unused_body_fail_then_repaired_tests_pass(self):
        native = runpy.run_path(str(Path(__file__).with_name("coverage_reports.py")))
        gate = runpy.run_path(str(Path(__file__).with_name("coverage_gate.py")))
        def run(args, **kwargs):
            return subprocess.check_output(list(map(str, args)), text=True, stderr=subprocess.STDOUT, timeout=30, **kwargs)
        sysroot = Path(run(["rustc", "--print", "sysroot"]).strip())
        host = next(line.removeprefix("host: ") for line in run(["rustc", "-vV"]).split("\n") if line.startswith("host: "))
        tools = sysroot / "lib/rustlib" / host / "bin"
        source = "\n".join([
            "pub fn live(x: Option<u32>) -> u32 {",
            "    x.map(|value| value + 1).unwrap_or(0)",
            "}",
            "pub fn dead() -> u32 { 41 }",
            "#[cfg(test)] mod tests {",
            "    #[test] fn calls_live() { assert_eq!(super::live(Some(1)), 2); }",
            "}", ""])
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            def stage(label, content, optimize):
                root = Path(directory) / label; root.mkdir()
                code, library, binary = root / "fixture.rs", root / "fixture.rlib", root / "fixture-tests"
                code.write_text(content)
                profiles = root / "profiles"; profiles.mkdir()
                flags = ["--edition=2024", "--crate-name=native_overlap_fixture", "-Cinstrument-coverage", "-Clink-dead-code", "-Copt-level=" + str(optimize)]
                run(["rustc", *flags, "--crate-type=rlib", code, "-o", library])
                run(["rustc", *flags, "--test", code, "-o", binary])
                env = dict(os.environ, LLVM_PROFILE_FILE=str(profiles / "%m-%p.profraw"), DEBUGINFOD_URLS="")
                self.assertIn("1 passed", run([binary], env=env))
                objects = []
                for index, member in enumerate(name for name in run(["ar", "t", library]).split("\n") if name.endswith(".o")):
                    path = root / (str(index) + ".o")
                    with path.open("wb") as output:
                        subprocess.run(["ar", "p", str(library), member], stdout=output, timeout=30, check=True)
                    objects.append(path)
                self.assertTrue(objects)
                baseline_args = [tools / "llvm-cov", "export", "--num-threads=2", "--empty-profile", objects[0],
                                 *[part for path in objects[1:] for part in ("--object", path)]]
                baseline_text = run([*baseline_args, "--format=text"])
                baseline_report = native["decode"](baseline_text)
                baseline = native["project"](baseline_report, {str(code)})
                self.assertEqual(baseline, {str(code): {1: 0, 2: 0, 3: 0, 4: 0}})
                # Independently read the actual LLVM DA coordinates; its old strict
                # parser must still reject a function-summary/physical mismatch.
                lcov = run([*baseline_args, "--format=lcov"])
                rows = lcov.split("\n")
                detailed = {int(row[3:].split(",")[0]): int(row.split(",")[1]) for row in rows if row.startswith("DA:")}
                self.assertEqual(detailed, baseline[str(code)])
                reported_count = int(next(row[3:] for row in rows if row.startswith("LF:")))
                mismatch = reported_count != 4
                if mismatch:
                    with self.assertRaisesRegex(ValueError, "Inconsistent LCOV file summary"):
                        gate["parse_lcov"](lcov)
                merged = root / "merged.profdata"
                raw_profiles = sorted(profiles.glob("*.profraw")); self.assertTrue(raw_profiles)
                run([tools / "llvm-profdata", "merge", "--sparse", "--num-threads=2", *raw_profiles, "-o", merged])
                runtime_args = [tools / "llvm-cov", "export", "--num-threads=2", "--instr-profile=" + str(merged), binary]
                runtime_text = run([*runtime_args, "--format=text"])
                runtime_report = native["decode"](runtime_text)
                runtime = native["project"](runtime_report, {str(code)})
                runtime_lcov = run([*runtime_args, "--format=lcov"])
                runtime_lines = {int(row[3:].split(",")[0]): int(row.split(",")[1]) for row in runtime_lcov.split("\n") if row.startswith("DA:")}
                self.assertEqual(runtime[str(code)], runtime_lines)
                metadata = {"fixture.rs": {"crate": "fixture", "sha256": hashlib.sha256(code.read_bytes()).hexdigest(),
                                           "line_count": content.count("\n")}}
                result = gate["evaluate"](metadata, {"fixture.rs": baseline[str(code)]}, {"fixture.rs": runtime[str(code)]},
                                          {"fixture.rs": [4]}, {}, ["fixture"])
                return result, {"baseline_raw_sha256": baseline_report.raw_sha256, "runtime_raw_sha256": runtime_report.raw_sha256,
                                "baseline_physical_lines": 4, "native_summary_count": reported_count, "legacy_refused_mismatch": mismatch}
            observed_mismatch = False
            repaired = source.replace("assert_eq!(super::live(Some(1)), 2);", "assert_eq!(super::live(Some(1)), 2); assert_eq!(super::live(None), 0); assert_eq!(super::dead(), 41);")
            for profile, optimize in (("dev", 0), ("release", 3)):
                failed, first = stage("uncovered-" + profile, source, optimize)
                passed, second = stage("repaired-" + profile, repaired, optimize)
                self.assertFalse(failed["passed"]); self.assertEqual(failed["aggregate"]["coverable"], 4)
                self.assertEqual(failed["aggregate"]["covered"], 3); self.assertEqual(failed["changed"]["covered"], 0)
                self.assertEqual(failed["changed"]["coverable"], 1)
                self.assertTrue(passed["passed"]); self.assertEqual(passed["aggregate"]["coverable"], 4)
                self.assertEqual(passed["aggregate"]["covered"], 4); self.assertEqual(passed["changed"]["covered"], 1)
                observed_mismatch |= first["legacy_refused_mismatch"]
                print(json.dumps({"native_physical_overlap": "pass", "profile": profile, "uncovered_seed": failed["aggregate"],
                                  "test_repair": passed["aggregate"], "unchanged_production_source": True,
                                  "native_receipts": [first, second], "protocol": native["PROTOCOL"]}), flush=True)
            self.assertTrue(observed_mismatch, "Actual overlap fixture must expose at least one native summary mismatch")


class NextestDeadlineControl(unittest.TestCase):
    def test_inventory_and_timeout_are_real_and_bounded(self):
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text('[package]\nname="coverage-nextest-fixture"\nversion="0.0.0"\nedition="2024"\n[workspace]\n')
            # This positive deliberately exceeds the negative-only100ms budget.
            # Production startup is not a100ms performance promise.
            (root / "src/lib.rs").write_text('#[test] fn fast() { std::thread::sleep(std::time::Duration::from_millis(300)); assert_eq!(2+2,4); }\n#[test] fn timeout_seed() { std::thread::sleep(std::time::Duration::from_secs(10)); }\n'
                '#[test] fn retry_seed() { let marker = std::path::PathBuf::from(std::env::var_os("HSEARCH_RETRY_SEED").unwrap()); '
                'if !marker.exists() { std::fs::write(marker, b"controlled initial failure").unwrap(); panic!("controlled initial failure"); } }\n')
            config = root / "nextest.toml"
            config.write_text('[profile.default]\nretries=0\ntest-threads=1\nslow-timeout={period="30s",terminate-after=4,grace-period="5s",on-timeout="fail"}\nglobal-timeout="3s"\n'
                '[[profile.default.overrides]]\nfilter="test(=timeout_seed)"\nslow-timeout={period="100ms",terminate-after=1,grace-period="100ms",on-timeout="fail"}\n')
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
            # Retain the original broad100ms fixture as a failing regression:
            # the successful300ms body is not the intentional timeout seed.
            wide_config = root / "wide-timeout-seed.toml"
            wide_config.write_text('[profile.default]\nretries=0\ntest-threads=1\nslow-timeout={period="100ms",terminate-after=1,grace-period="100ms",on-timeout="fail"}\nglobal-timeout="3s"\n')
            wide = subprocess.run([*base, "run", "--manifest-path", str(root / "Cargo.toml"),
                                   "--locked", "--offline", "--config-file", str(wide_config), "-E", "test(=fast)"],
                                  text=True, timeout=10, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            self.assertEqual(wide.returncode, 100, wide.stdout)
            self.assertIn("TIMEOUT", wide.stdout)
            passing = subprocess.run([*base, "run", *common, "-E", "test(=fast)"], text=True, timeout=10, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            self.assertEqual(passing.returncode, 0, passing.stdout)
            self.assertIn("1 passed", passing.stdout)
            started = time.monotonic()
            failed = subprocess.run([*base, "run", *common, "-E", "test(=timeout_seed)"], text=True, timeout=10, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            self.assertNotEqual(failed.returncode, 0)
            self.assertEqual(failed.returncode, 100, failed.stdout)
            self.assertIn("TIMEOUT", failed.stdout)
            self.assertRegex(failed.stdout, r"TERMINATING\s+\[>\s+0\.100s\]")
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
            print(json.dumps({"nextest_inventory": "pass", "tests": 3, "positive_test": "pass", "positive_body_delay_ms": 300, "wide_positive_timeout": "reproduced", "timeout_scope": "only_timeout_seed", "timeout_seed_ms": 100, "timeout_seed": "rejected", "exit": failed.returncode,
                              "retry_override_false_success_observed": True, "explicit_no_retry_seed": "rejected", "production_override": "refused"}), flush=True)


class ActualCompactExportControls(unittest.TestCase):
    def test_pinned_full_and_compact_exports_have_identical_file_maps(self):
        cov = runpy.run_path(str(Path(__file__).with_name("coverage.py")))
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            parent = Path(directory)
            source = parent / "profile.rs"
            source.write_text('macro_rules! bump { ($x:expr) => { $x + 1 } }\n#[inline(never)] fn generic<T>(x: u32) -> u32 { let _ = std::mem::size_of::<T>(); bump!(x) }\nfn main() { assert_eq!(generic::<u8>(1) + generic::<u64>(2), 5); }\n', encoding="utf-8")
            for profile, optimization in [("dev", "0"), ("release", "3")]:
                runner = cov["Runner"](parent / profile)
                tools, host, llvm = cov["tool_identity"](runner)
                binary = runner.root / "native-profile"
                runner.command([tools["cargo_compiler"], "--edition=2024", "--target", host,
                                "-Cinstrument-coverage", "-Ccodegen-units=1", "-Copt-level=" + optimization,
                                str(source), "-o", binary])
                profiles = runner.root / "runtime-profiles"
                profiles.mkdir()
                environment = dict(runner.env, LLVM_PROFILE_FILE=str(profiles / "%m-%p.profraw"))
                runner.command([tools["test_executor"]["path"], "--", binary], env=environment)
                raw = sorted(profiles.glob("*.profraw"))
                self.assertTrue(raw)
                merged = runner.root / "actual.profdata"
                runner.command([str(llvm / "llvm-profdata"), "merge", "--sparse", "--num-threads=2", *raw, "-o", merged])
                receipts = []
                for scope, data in [("baseline", None), ("runtime", merged)]:
                    compact_command = cov["native_export_command"](llvm, [binary], profile_path=data)
                    full_command = [arg for arg in compact_command if arg != "--skip-functions"]
                    full_text = runner.command(full_command)
                    compact_text = runner.command(compact_command)
                    full_doc, compact_doc = json.loads(full_text), json.loads(compact_text)
                    self.assertTrue(full_doc["data"][0]["functions"])
                    self.assertNotIn("functions", compact_doc["data"][0])
                    self.assertEqual(full_doc["data"][0]["files"], compact_doc["data"][0]["files"])
                    self.assertEqual(full_doc["data"][0]["totals"], compact_doc["data"][0]["totals"])
                    full_map = cov["REPORTS"]["decode"](full_text)
                    compact_map = cov["REPORTS"]["decode"](compact_text, profile="files")
                    names = {item.filename for item in full_map.files}
                    self.assertTrue(str(source) in names)
                    self.assertEqual(full_map.files, compact_map.files)
                    self.assertEqual(cov["REPORTS"]["project"](full_map, names), cov["REPORTS"]["project"](compact_map, names))
                    with self.assertRaisesRegex(ValueError, "Native export shape"):
                        cov["REPORTS"]["decode"](compact_text)
                    receipts.append({"scope": scope, "files": len(full_map.files), "full_bytes": len(full_text.encode()),
                                     "compact_bytes": len(compact_text.encode()), "full_raw_sha256": full_map.raw_sha256,
                                     "compact_raw_sha256": compact_map.raw_sha256})
                print(json.dumps({"native_compact_export": "pass", "profile": profile, "target": host,
                                  "all_file_records_and_totals_identical": True, "physical_projection_identical": True,
                                  "legacy_full_decoder_still_strict": True, "file_filters": False,
                                  "raw_report_limit_bytes": cov["REPORTS"]["MAX_BYTES"], "receipts": receipts}), flush=True)


if __name__ == "__main__":
    if platform.system() != "Linux":
        raise SystemExit("Compiler coverage/deadline corpus requires qualified Linux")
    parent = Path(os.environ["CARGO_TARGET_DIR"]).absolute()
    if parent.resolve() != parent or parent.is_relative_to(Path(__file__).resolve().parents[1]):
        raise SystemExit("Compiler corpus target must be private and outside source")
    parent.mkdir(parents=True, exist_ok=True)
    unittest.main()
