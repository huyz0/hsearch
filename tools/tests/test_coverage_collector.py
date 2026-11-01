"""Collection must retain declared targets, finite policies and primary failures."""
from pathlib import Path
import copy
from contextlib import contextmanager
import json
import os
import runpy
import subprocess
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
COV = runpy.run_path(str(ROOT / "tools/coverage.py"))


@contextmanager
def repository(files):
    """Actual original byte/blob fixtures; no human patch stream is mocked."""
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory).resolve()
        assert root.is_relative_to(Path(tempfile.gettempdir()).resolve())
        def git(*args):
            return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.PIPE,
                                           timeout=20).decode("utf-8").strip()
        git("init", "-q"); git("config", "user.name", "coverage source fixture")
        git("config", "user.email", "fixture@example.invalid"); git("config", "core.autocrlf", "false")
        for name, raw in files.items():
            path = root / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_bytes(raw)
        git("add", "."); git("commit", "-qm", "original")
        with patch.dict(COV["changed_lines"].__globals__, ROOT=root):
            yield root, git


def metadata():
    return {"fixture": {"id": "fixture-id", "targets": [{"name": "fixture", "kind": ["lib"], "test": True},
                                                           {"name": "integration", "kind": ["test"], "test": True}]}}


def inventory():
    return {"rust-suites": {name: {"binary-id": name, "package-id": "fixture-id", "binary-name": name,
                                  "kind": kind, "status": "listed", "testcases": {"original": {"ignored": False, "filter-match": {"status": "matches"}}}}
                            for name, kind in [("fixture", "lib"), ("integration", "test")]}}


class CollectionInventoryControls(unittest.TestCase):
    def test_merge_source_inventory_uses_actual_nul_records_including_unicode_names(self):
        names = {'crates/a/src/lib.rs': b'pub fn original() -> u32 { 42 }\n',
                 'crates/a/src/line\u2028name.rs': b'pub const DECL: u32 = 1;\n'}
        with repository(names) as (root, _git):
            paths = COV['tracked_paths']()
            self.assertEqual(paths, set(names))
            packages = {'a': {'manifest_path': str(root / 'crates/a/Cargo.toml'),
                              'targets': [{'kind': ['lib'], 'src_path': str(root / 'crates/a/src/lib.rs')}]}}
            inventory = COV['initial_sources'](packages, ['a'], paths)
            self.assertEqual(set(inventory), set(names))
            self.assertTrue(all(record['crate'] == 'a' and record['line_count'] == 1 for record in inventory.values()))
        for malformed in ['original.rs', 'original.rs\0\0', 'original.rs\0original.rs\0']:
            with patch.dict(COV['tracked_paths'].__globals__, TASK={'git': lambda *_args, **_kwargs: malformed}), self.assertRaises(ValueError):
                COV['tracked_paths']()

    def test_exact_unicode_counterexample_cannot_hide_uncovered_body_as_changed_na(self):
        fixture = json.loads((ROOT / 'docs/plan/evidence/coverage-unicode-counterexample.json').read_text(encoding='utf-8'))
        with repository({name: raw.encode('utf-8') for name, raw in fixture['before'].items()}) as (root, git):
            base = git('rev-parse', 'HEAD')
            for name, raw in fixture['after'].items(): (root / name).write_bytes(raw.encode('utf-8'))
            a, b = 'crates/a/src/lib.rs', 'crates/b/src/lib.rs'
            (root / b).chmod(0o755); git('add', '.'); git('update-index', '--chmod=+x', b)
            git('commit', '-qm', 'original exact Unicode counterexample')
            sources = {name: COV['source_metadata'](name, owner) for name, owner in [(a, 'a'), (b, 'b')]}
            proof = COV['source_proof'](base, sources)
            self.assertEqual({name: record['added_head_lines'] for name, record in proof['files'].items()},
                             fixture['expected_added_lines'])
            changed = COV['verify_source_proof'](base, sources, proof)
            raw_lines = fixture['after'][a].split('\n')
            live = {i: 1 for i, line in enumerate(raw_lines, 1) if line.startswith('pub fn live')}
            body = raw_lines.index('42') + 1
            self.assertEqual(body, 24)
            baseline = {a: {line: 0 for line in [*live, body]}}
            audit = {b: {'sha256': sources[b]['sha256'], 'reason': 'Original audited declaration-only fixture.'}}
            report = COV['GATE']['evaluate'](sources, baseline, {a: live}, changed, audit, ['a', 'b'])
            self.assertEqual((report['aggregate']['covered'], report['aggregate']['coverable']), (10, 11))
            self.assertEqual(report['aggregate']['status'], 'pass')
            self.assertEqual((report['changed']['covered'], report['changed']['coverable']), (0, 1))
            self.assertEqual(report['changed']['status'], 'fail')
            self.assertFalse(report['passed'])

    def test_identical_mode_only_blob_has_zero_delta_and_oversized_physical_inventory_refuses(self):
        with repository({"original.rs": b"one\ntwo\nthree\n"}) as (root, git):
            base = git("rev-parse", "HEAD")
            (root / "original.rs").chmod(0o755)
            git("update-index", "--chmod=+x", "original.rs"); git("commit", "-qm", "mode only")
            sources = {"original.rs": COV["source_metadata"]("original.rs", "fixture")}
            proof = COV["source_proof"](base, sources)
            self.assertEqual(proof["files"]["original.rs"]["added_head_lines"], [])
            self.assertEqual(COV["changed_lines"](base, sources), {})
            (root / "large.rs").write_bytes(b"\n" * 2_000_001)
            with self.assertRaisesRegex(ValueError, "physical line count exceeds bound"):
                COV["source_metadata"]("large.rs", "fixture")

    def test_truncated_source_proof_cannot_become_empty_success(self):
        with repository({"original.rs": b"first\n"}) as (root, git):
            base = git("rev-parse", "HEAD")
            (root / "original.rs").write_bytes(b"first\nsecond\n")
            git("add", "."); git("commit", "-qm", "second original line")
            sources = {"original.rs": COV["source_metadata"]("original.rs", "fixture")}
            proof = COV["source_proof"](base, sources)
            self.assertEqual(proof["files"]["original.rs"]["added_head_lines"], [2])
            for variant in ["file", "blob", "line"]:
                broken = copy.deepcopy(proof)
                if variant == "file": broken["files"] = {}
                elif variant == "blob": broken["files"]["original.rs"].pop("head_blob")
                else: broken["files"]["original.rs"]["added_head_lines"] = []
                with self.subTest(variant=variant), self.assertRaisesRegex(ValueError, "source_proof_mismatch"):
                    COV["verify_source_proof"](base, sources, broken)

    def test_only_explicit_dependency_roots_may_exclude_external_maps(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory).resolve(); root = parent / 'checkout'; root.mkdir()
            vendor = parent / 'pinned-vendor'; vendor.mkdir()
            source = vendor / 'original.rs'; source.write_text('pub fn dependency() {}\n')
            report = f'SF:{source.as_posix()}\nDA:1,0\nLF:1\nLH:0\nend_of_record\n'
            with patch.dict(COV['normalize_report'].__globals__, ROOT=root):
                self.assertEqual(COV['normalize_report'](report, {}, {}, [], set(), external_roots=[str(vendor)]), {})
                (vendor / 'src').mkdir()
                alias = report.replace(source.as_posix(), (vendor / 'src/../original.rs').as_posix())
                self.assertEqual(COV['normalize_report'](alias, {}, {}, [], set(), external_roots=[str(vendor)]), {})
                escaped = report.replace(source.as_posix(), (vendor / '../unowned.rs').as_posix())
                with self.assertRaisesRegex(ValueError, 'Unowned external'):
                    COV['normalize_report'](escaped, {}, {}, [], set(), external_roots=[str(vendor)])
                with self.assertRaisesRegex(ValueError, 'Unowned external'):
                    COV['normalize_report'](report, {}, {}, [], set(), external_roots=[str(parent / 'different-vendor')])

    def test_production_map_inside_known_dev_package_is_not_a_development_exclusion(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            prod, dev = root / 'crates/prod', root / 'crates/testkit'
            (prod / 'src').mkdir(parents=True); (dev / 'src').mkdir(parents=True)
            (prod / 'src/lib.rs').write_text('pub fn live() {}\n')
            helper = dev / 'src/helper.rs'; helper.write_text('pub fn uncovered() {}\n')
            report = f'SF:{helper.as_posix()}\nDA:1,0\nLF:1\nLH:0\nend_of_record\n'
            packages = {name: {'manifest_path': str(path / 'Cargo.toml')} for name, path in [('prod', prod), ('hsearch-testkit', dev)]}
            with patch.dict(COV['normalize_report'].__globals__, ROOT=root):
                with self.assertRaisesRegex(ValueError, 'Unowned first-party'):
                    COV['normalize_report'](report, {}, packages, ['prod'], {'crates/testkit/src/helper.rs'})
                self.assertEqual(COV['normalize_report'](report, {}, packages, ['prod'], {'crates/testkit/src/helper.rs'}, runtime=True), {})

    def test_unknown_source_outside_checkout_cannot_impersonate_a_dependency(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory).resolve(); root = parent / 'checkout'; root.mkdir()
            external = parent / 'unowned.rs'; external.write_text('pub fn hidden() {}\n')
            report = f'SF:{external.as_posix()}\nDA:1,0\nLF:1\nLH:0\nend_of_record\n'
            with patch.dict(COV['normalize_report'].__globals__, ROOT=root), self.assertRaisesRegex(ValueError, 'Unowned external'):
                COV['normalize_report'](report, {}, {'prod': {'manifest_path': str(root / 'prod/Cargo.toml')}}, ['prod'], set())

    @unittest.skipUnless(shutil.which('git'), 'Native Git is required for actual attribute/context controls')
    def test_diff_attributes_and_inherited_context_cannot_hide_uncovered_changed_line(self):
        for attribute, context in [('*.rs -diff\n', ''), ('', '--unified=100')]:
            with self.subTest(attribute=attribute, context=context), tempfile.TemporaryDirectory() as directory:
                root = Path(directory).resolve()
                def git(*args):
                    return subprocess.check_output(['git', *args], cwd=root, text=True, stderr=subprocess.PIPE, timeout=20).strip()
                git('init', '-q'); git('config', 'user.name', 'coverage fixture'); git('config', 'user.email', 'fixture@example.invalid')
                git('config', 'core.autocrlf', 'false')
                name = 'crates/a/src/lib.rs'; (root / name).parent.mkdir(parents=True)
                original = ''.join(f'pub fn live{i}() -> u32 {{ {i} }}\n' for i in range(10))
                (root / name).write_text(original); (root / '.gitattributes').write_text(attribute)
                git('add', '.'); git('commit', '-qm', 'original'); base = git('rev-parse', 'HEAD')
                (root / name).write_text(original + 'pub fn uncovered() -> u32 { 42 }\n')
                git('add', '.'); git('commit', '-qm', 'uncovered addition')
                with patch.dict(COV['changed_lines'].__globals__, ROOT=root), patch.dict(os.environ, GIT_DIFF_OPTS=context):
                    source = COV['source_metadata'](name, 'a')
                    changed = COV['changed_lines'](base, {name: source})
                    report = COV['GATE']['evaluate']({name: source}, {name: {line: 0 for line in range(1, 12)}},
                        {name: {line: 1 for line in range(1, 11)}}, changed, {}, ['a'])
                    self.assertEqual(report['aggregate']['covered'], 10)
                    self.assertGreater(report['aggregate']['percent'], 90)
                    self.assertEqual(report['changed']['coverable'], 1)
                    self.assertEqual(report['changed']['covered'], 0)
                    self.assertFalse(report['passed'])

    def test_cargo_compiler_is_pinned_and_conflicting_selection_refuses(self):
        with tempfile.TemporaryDirectory() as directory:
            compiler = Path(directory).resolve() / "rustc"
            compiler.write_bytes(b"inert pinned compiler identity fixture")
            class Runner: env = {}
            with patch.dict(os.environ, RUSTC=str(compiler)):
                COV["bind_compiler"](Runner(), compiler)
            self.assertEqual(Runner.env["RUSTC"], str(compiler))
            with patch.dict(os.environ, RUSTC=str(compiler.parent / "conflicting")), self.assertRaisesRegex(ValueError, "RUSTC"):
                COV["bind_compiler"](Runner(), compiler)

    def test_even_empty_encoded_flags_cannot_remove_instrumentation(self):
        with patch.dict(os.environ, CARGO_ENCODED_RUSTFLAGS=""), self.assertRaisesRegex(ValueError, "CARGO_ENCODED_RUSTFLAGS"):
            COV["tool_identity"](object())  # Refuse before executing any tool.

    def test_modified_or_redirected_compiler_child_policy_refuses(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory).resolve() / "policy.toml"
            path.write_bytes(COV["COMPILER_POLICY_BYTES"].replace(b'"-1"', b'"1" '))
            with patch.dict(COV["cargo_policy"].__globals__, COMPILER_POLICY=path), self.assertRaisesRegex(ValueError, "compiler-child"):
                COV["cargo_policy"]()

    def test_ordinary_targets_are_fresh_and_preserve_unqualified_outputs(self):
        with tempfile.TemporaryDirectory() as directory:
            shared = Path(directory).resolve() / "shared"
            shared.mkdir()
            original = shared / "unqualified-artifact"
            original.write_bytes(b"preserved pre-policy artifact")
            with patch.dict(os.environ, CARGO_TARGET_DIR=str(shared)), patch.dict(
                    COV["qualified_tools"].__globals__, tool_identity=lambda _runner: ({}, None, None)):
                first, second = COV["qualified_tools"](), COV["qualified_tools"]()
            targets = [Path(value["ordinary_target"]) for value in (first, second)]
            self.assertNotEqual(*targets)
            self.assertTrue(all(path.is_relative_to(shared) and path.is_dir() and not list(path.iterdir()) for path in targets))
            self.assertEqual(original.read_bytes(), b"preserved pre-policy artifact")

    def test_nextest_environment_cannot_override_fixed_policy(self):
        for name, value in [("NEXTEST_RETRIES", "1"), ("NEXTEST_TEST_THREADS", "99")]:
            with patch.dict(os.environ, {name: value}), self.assertRaisesRegex(ValueError, "[Ee]nvironment|[Oo]verride"):
                COV["nextest_policy"]()

    def test_unowned_checkout_production_map_must_not_disappear(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            package = root / "crates/prod"
            (package / "src").mkdir(parents=True)
            (package / "src/lib.rs").write_text('pub fn live() {}\n')
            (root / "shared.rs").write_text('pub fn uncovered() {}\n')
            report = ''.join(f'SF:{path.as_posix()}\nDA:1,0\nLF:1\nLH:0\nend_of_record\n'
                             for path in [package / "src/lib.rs", root / "shared.rs"])
            with patch.dict(COV["normalize_report"].__globals__, ROOT=root), self.assertRaisesRegex(ValueError, "[Uu]nowned first-party"):
                COV["normalize_report"](report, {}, {"prod": {"manifest_path": str(package / "Cargo.toml")}}, ["prod"],
                                        {"crates/prod/src/lib.rs", "shared.rs"})

    @unittest.skipUnless(shutil.which("git"), "Native Git is required for the actual diff-content control")
    def test_real_raw_string_diff_content_cannot_switch_changed_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True, stderr=subprocess.PIPE, timeout=20).strip()
            git("init", "-q"); git("config", "user.name", "coverage fixture"); git("config", "user.email", "fixture@example.invalid")
            git("config", "core.autocrlf", "false")
            a, b = "crates/a/src/lib.rs", "crates/b/src/lib.rs"
            (root / a).parent.mkdir(parents=True); (root / b).parent.mkdir(parents=True)
            original = 'pub const ORIGINAL: &str = r#"\noriginal\n"#;\n' + '\n' * 4 + ''.join(f'pub fn live{i}() -> u32 {{ {i} }}\n' for i in range(10)) + '\n' * 4
            (root / a).write_text(original)
            (root / b).write_text('pub trait Declaration {}\n' + '\n' * 50)
            git("add", "."); git("commit", "-qm", "original"); base = git("rev-parse", "HEAD")
            changed = original.replace('original\n', 'original\n++ b/' + b + '\n') + 'pub fn uncovered() -> u32 {\n42\n}\n'
            (root / a).write_text(changed); git("add", "."); git("commit", "-qm", "raw string and uncovered function")
            with patch.dict(COV["changed_lines"].__globals__, ROOT=root):
                sources = {name: COV["source_metadata"](name, crate) for name, crate in [(a, "a"), (b, "b")]}
                observed = COV["changed_lines"](base, sources)
                body = changed.splitlines().index('42') + 1
                self.assertIn(body, observed[a])
                self.assertNotIn(b, observed)
                live = {index + 1: 1 for index, line in enumerate(changed.splitlines()) if line.startswith('pub fn live')}
                report = COV["GATE"]["evaluate"](sources, {a: {line: 0 for line in [*live, body]}}, {a: live}, observed,
                    {b: {"sha256": sources[b]["sha256"], "reason": "Audited declaration-only original fixture with no executable body."}}, ["a", "b"])
                self.assertGreater(report["aggregate"]["percent"], 90)
                self.assertEqual(report["changed"]["covered"], 0)
                self.assertFalse(report["passed"])

    def test_all_physical_lines_require_proven_root_and_missing_base_never_dilutes(self):
        with repository({"original.rs": b"one\ntwo\nthree\n"}) as (root, git):
            sources = {"original.rs": COV["source_metadata"]("original.rs", "fixture")}
            self.assertEqual(COV["changed_lines"]("parentless-root", sources), {"original.rs": [1, 2, 3]})
            git("commit", "--allow-empty", "-qm", "nonroot with identical source")
            for base in ["all-production", "", None, "0" * 40, "parentless-root"]:
                with self.subTest(base=base), self.assertRaises(ValueError):
                    COV["changed_lines"](base, sources)

    def test_listing_profiles_always_have_a_fresh_private_destination(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = COV["Runner"](Path(directory) / "context")
            destination = Path(runner.env["LLVM_PROFILE_FILE"])
            self.assertEqual(destination.parent.parent, runner.root)
            self.assertTrue(destination.parent.is_dir())
            reopened = COV["Runner"](runner.root, create=False)
            self.assertNotEqual(reopened.env["LLVM_PROFILE_FILE"], runner.env["LLVM_PROFILE_FILE"])
            self.assertTrue(destination.parent.is_dir())

    def test_complete_declared_test_inventory_passes(self):
        self.assertEqual(len(COV["verify_test_inventory"](inventory(), metadata(), ["fixture"])), 2)

    def test_missing_ignored_filtered_and_unlisted_targets_refuse(self):
        for change in ("missing", "ignored", "filtered", "unlisted", "wrong_kind"):
            value = inventory()
            if change == "missing": value["rust-suites"].pop("integration")
            if change == "ignored": value["rust-suites"]["fixture"]["testcases"]["original"]["ignored"] = True
            if change == "filtered": value["rust-suites"]["fixture"]["testcases"]["original"]["filter-match"]["status"] = "mismatch"
            if change == "unlisted": value["rust-suites"]["fixture"]["status"] = "skipped"
            if change == "wrong_kind": value["rust-suites"]["integration"]["kind"] = "lib"
            with self.subTest(change=change), self.assertRaises(ValueError):
                COV["verify_test_inventory"](value, metadata(), ["fixture"])

    def test_nextest_retry_timeout_and_success_on_timeout_overrides_refuse(self):
        original = (ROOT / ".config/nextest.toml").read_text()
        globals_ = COV["nextest_policy"].__globals__
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".config").mkdir()
            with patch.dict(globals_, ROOT=root):
                (root / ".config/nextest.toml").write_text(original)
                COV["nextest_policy"]()
                for changed in [original.replace('retries = 0', 'retries = 1'), original.replace('on-timeout = "fail"', 'on-timeout = "pass"'),
                                original.replace('global-timeout = "15m"', 'global-timeout = "150m"')]:
                    (root / ".config/nextest.toml").write_text(changed)
                    with self.assertRaises(ValueError): COV["nextest_policy"]()

    def test_immutable_blobs_count_original_added_lines_without_context_or_deleted_lines(self):
        name = "crates/a/src/lib.rs"
        before = [("stable " + str(i) + "\n").encode() for i in range(1, 101)]
        before[1:3] = [b"old\n", b"older\n"]; before[19] = b"removed\n"
        with repository({name: b"".join(before)}) as (root, git):
            base = git("rev-parse", "HEAD")
            after = before[:]; after[1:3] = [b"new\n", b"second\n", b"third\n"]; del after[20]
            (root / name).write_bytes(b"".join(after)); git("add", "."); git("commit", "-qm", "same original delta oracle")
            sources = {name: COV["source_metadata"](name, "a")}
            self.assertEqual(COV["changed_lines"](base, sources), {name: [2, 3, 4]})

    def test_compiler_artifact_outside_private_context_refuses(self):
        with tempfile.TemporaryDirectory() as directory:
            class Runner:
                root = Path(directory) / "private"
            Runner.root.mkdir()
            other = Path(directory) / "unowned"
            other.write_bytes(b"retain original fixture")
            with self.assertRaises(ValueError): COV["owned_artifact"](Runner(), str(other))
            self.assertEqual(other.read_bytes(), b"retain original fixture")


@unittest.skipIf(os.name == "nt", "Native process groups are qualified inside Linux")
class NativeCommandControls(unittest.TestCase):
    def test_dead_leader_retained_pipe_stops_owned_descendant_and_preserves_bystander(self):
        import signal
        import time
        with tempfile.TemporaryDirectory() as directory:
            runner = COV["Runner"](Path(directory) / "context")
            receipt = Path(directory) / "descendant.json"
            program = ("import json,os,subprocess,sys;from pathlib import Path;"
                       "child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)']);"
                       "Path(sys.argv[1]).write_text(json.dumps({'group':os.getpgrp(),'child':child.pid}))")
            bystander = subprocess.Popen([sys.executable, "-c", "import time;time.sleep(30)"], start_new_session=True)
            try:
                with self.assertRaisesRegex(ValueError, "deadline"):
                    runner.command([sys.executable, "-c", program, str(receipt)], timeout=0.5)
                state = json.loads(receipt.read_text())
                deadline = time.monotonic() + 1
                while time.monotonic() < deadline:
                    proc = Path('/proc') / str(state['child']) / 'stat'
                    if not proc.exists() or proc.read_text().split(') ', 1)[1].split()[0] == 'Z': break
                    time.sleep(0.01)
                else: self.fail('Owned descendant remains running after exited-leader deadline')
                self.assertIsNone(bystander.poll())
            finally:
                if receipt.exists():
                    state = json.loads(receipt.read_text())
                    try:
                        if os.getpgid(state['child']) == state['group']: os.killpg(state['group'], signal.SIGKILL)
                    except ProcessLookupError: pass
                bystander.terminate(); bystander.wait(timeout=5)
    def test_output_and_deadline_failures_stop_only_owned_process_group(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = COV["Runner"](Path(directory) / "context")
            bystander = subprocess.Popen([sys.executable, "-c", "import time;time.sleep(30)"], start_new_session=True)
            try:
                for command, timeout, limit in [("print('x'*1000000)", 5, 1024), ("import time;time.sleep(30)", 0.05, 1024)]:
                    with self.assertRaises(ValueError): runner.command([sys.executable, "-c", command], timeout=timeout, limit=limit)
                    self.assertIsNone(bystander.poll())
            finally:
                bystander.terminate(); bystander.wait(timeout=5)

    def test_primary_failure_survives_log_write_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            runner = COV["Runner"](Path(directory) / "context")
            primary = RuntimeError("controlled primary monitor failure")
            def fail(_paths): raise primary
            with patch.dict(COV["Runner"].command.__globals__["EXECUTE"], usage=fail), patch.object(Path, "write_bytes", side_effect=OSError("controlled full log sink")):
                with self.assertRaises(RuntimeError) as caught:
                    runner.command([sys.executable, "-c", "import time;time.sleep(30)"], timeout=1)
            self.assertIs(caught.exception, primary)
