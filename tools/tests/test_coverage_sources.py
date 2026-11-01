"""Independent small DP oracle, manual attribution and actual immutable Git cases."""
import copy
from dataclasses import replace
import hashlib
import itertools
import json
import os
from pathlib import Path
import random
import runpy
import platform
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
MOD = runpy.run_path(str(ROOT / 'tools/coverage_sources.py'))
Refused, Limits, Budget, delta, prove, verify, manifest = (MOD[name] for name in
    ['Refused', 'Limits', 'Budget', 'added_lines', 'prove', 'verify', 'manifest_record'])


def reference_lines(raw):
    # Intentionally independent: scan physical LF end offsets rather than split.
    result, start = [], 0
    for index, byte in enumerate(raw):
        if byte == 10:
            result.append(raw[start:index + 1]); start = index + 1
    if start < len(raw): result.append(raw[start:])
    return result


def lcs_length(left, right):
    previous = [0] * (len(right) + 1)
    for a in left:
        current = [0]
        for j, b in enumerate(right):
            current.append(previous[j] + 1 if a == b else max(previous[j + 1], current[-1]))
        previous = current
    return previous[-1]


class DeltaOracle(unittest.TestCase):
    @unittest.skipUnless(platform.system() == 'Linux', 'Native process RSS/cost control requires Linux')
    def test_large_original_native_delta_cost_and_repetitive_refusal(self):
        # A fresh interpreter exposes actual algorithm RSS/time without suite noise.
        code = '''import json,resource,runpy,time
m=runpy.run_path('tools/coverage_sources.py'); b=m['Budget'](); before=b''.join((str(i).encode()+b' original line\\n') for i in range(200000)); parts=before.split(b'\\n');parts[100000]=b'changed original line';after=b'\\n'.join(parts)+b'appended\\n'; start=time.monotonic();added=m['added_lines'](before,after,b);assert added==[100001,200001];print(json.dumps({'source_delta_native_cost':{'physical_input_lines':400001,'input_bytes':len(before)+len(after),'added':added,'seconds':time.monotonic()-start,'peak_rss_bytes':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss*1024,'operations':b.operations,'stored_frontier_cells':b.cells}}))
'''
        result = subprocess.run([sys.executable, '-c', code], cwd=ROOT, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, timeout=15, check=True)
        self.assertLess(len(result.stdout) + len(result.stderr), 8192)
        observation = json.loads(result.stdout)
        self.assertGreater(observation['source_delta_native_cost']['peak_rss_bytes'], 0)
        print(json.dumps(observation), flush=True)
        before = b''.join((str(i).encode() + b'\n') for i in range(10000))
        after = b''.join((str(i).encode() + b'\n') for i in reversed(range(10000)))
        started = time.monotonic()
        with self.assertRaisesRegex(Refused, 'delta_frontier_limit'):
            delta(before, after)
        self.assertLess(time.monotonic() - started, 15)

    def test_physical_framing_manual_final_lf_cr_and_separators(self):
        for raw, expected in [(b'', []), (b'\n', [b'\n']), (b'a\n', [b'a\n']),
                              (b'a\n\n', [b'a\n', b'\n']), (b'a', [b'a']), (b'\r\n', [b'\r\n'])]:
            self.assertEqual(MOD['physical_lines'](raw), expected)
        for separator in ['\x85', '\u2028', '\u2029', '\r', '\v', '\f']:
            raw = ('first' + separator + '+++ fake\nlast\n').encode('utf-8')
            self.assertEqual(MOD['physical_lines'](raw), reference_lines(raw))
            self.assertEqual(len(reference_lines(raw)), 2)
            self.assertEqual(delta(b'first\nlast\n', raw), [1])
        self.assertEqual(delta(b'a\n', b'a'), [1])
        self.assertEqual(delta(b'a\r\n', b'a\n'), [1])
        self.assertEqual(delta(b'a\nb\n', b'b\na\n'), [2])  # Deletion wins frontier tie.
        self.assertEqual(delta(b'a\n', b'a\na\n'), [2])

    def test_exhaustive_small_binary_lines_against_independent_dp(self):
        sequences = [b''.join(parts) for n in range(5) for parts in itertools.product([b'a\n', b'b\n'], repeat=n)]
        for before, after in itertools.product(sequences, repeat=2):
            self.check_oracle(before, after)

    def check_oracle(self, before, after):
        added = delta(before, after)
        old, new = reference_lines(before), reference_lines(after)
        self.assertEqual(added, sorted(set(added)))
        self.assertTrue(all(type(i) is int and 1 <= i <= len(new) for i in added))
        kept = [line for i, line in enumerate(new, 1) if i not in added]
        self.assertEqual(len(kept), lcs_length(old, new))
        # Removed additions must be an actual subsequence, not just the right count.
        self.assertEqual(lcs_length(old, kept), len(kept))

    def test_fixed_seed_original_bytes_repetitions_and_no_final_lf(self):
        randomizer = random.Random(20261101)
        alphabet = [b'', b'a', b'+++ b/fake', b'@@ -0,0 +1,1 @@', b'\x0b\x0c\r', '\u2028\u2029\x85'.encode()]
        for _ in range(500):
            raw = []
            for _side in range(2):
                value = b'\n'.join(randomizer.choice(alphabet) for _ in range(randomizer.randrange(12)))
                if randomizer.randrange(2): value += b'\n'
                raw.append(value)
            self.check_oracle(*raw)

    def test_forced_byte_line_work_frontier_and_deadline_failures(self):
        for limits, before, after, reason in [
            (replace(Limits(), blob_bytes=2), b'aaa', b'', 'blob_byte_limit'),
            (replace(Limits(), total_bytes=3), b'aa', b'aa', 'total_byte_limit'),
            (replace(Limits(), lines=1), b'a\n', b'b\n', 'line_inventory_limit'),
            (replace(Limits(), operations=2), b'a\nb\nc\n', b'x\ny\nz\n', 'delta_work_limit'),
            (replace(Limits(), frontier_cells=1), b'a\n', b'b\n', 'delta_frontier_limit')]:
            with self.assertRaisesRegex(Refused, reason): delta(before, after, Budget(limits))
        budget = Budget(); budget.deadline = time.monotonic() - 1
        with self.assertRaisesRegex(Refused, 'proof_deadline'): delta(b'', b'', budget)
        for invalid in [replace(Limits(), lines=True), replace(Limits(), seconds=61)]:
            with self.assertRaisesRegex(Refused, 'invalid_limit'): Budget(invalid)


class ImmutableSources(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.git('init', '-q')
        self.git('config', 'user.name', 'Original source proof fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.git('config', 'core.autocrlf', 'false')
        self.write('src/a.rs', b'pub fn old() {}\n')
        self.git('add', '.'); self.git('commit', '-qm', 'root')
        self.base = self.git('rev-parse', 'HEAD').decode().strip()

    def git(self, *arguments):
        return subprocess.check_output(['git', *arguments], cwd=self.root, stderr=subprocess.PIPE, timeout=20)

    def write(self, path, raw):
        file = self.root / path; file.parent.mkdir(parents=True, exist_ok=True); file.write_bytes(raw)

    def commit(self):
        self.git('add', '.'); self.git('commit', '-qm', 'head')
        return self.git('rev-parse', 'HEAD').decode().strip()

    def sources(self, *paths):
        return {path: manifest((self.root / path).read_bytes(), 'production-fixture') for path in paths}

    def test_actual_blob_bindings_attributes_context_names_and_modes(self):
        self.write('src/a.rs', b'pub fn old() {}\npub fn new() {\n42\n}\n')
        unicode_path = 'src/space \u2028 \u96ea.rs'
        self.write(unicode_path, b'new\r\n')
        self.write('.gitattributes', b'*.rs -diff\n')
        head = self.commit()
        sources = self.sources('src/a.rs', unicode_path)
        with patch.dict(os.environ, GIT_DIFF_OPTS='--unified=100', GIT_EXTERNAL_DIFF='must-never-execute'):
            proof = prove(self.root, head=head, base=self.base, sources=sources)
        self.assertEqual(proof['files']['src/a.rs']['added_head_lines'], [2, 3, 4])
        self.assertEqual(proof['files'][unicode_path]['added_head_lines'], [1])
        self.assertTrue(verify(proof, self.root, head=head, base=self.base, sources=sources))
        broken = copy.deepcopy(proof); broken['files']['src/a.rs']['added_head_lines'] = []
        with self.assertRaisesRegex(Refused, 'source_proof_mismatch'):
            verify(broken, self.root, head=head, base=self.base, sources=sources)
        self.assertNotIn('coverage', proof)

    def test_preserved_unicode_false_pass_exact_manual_attribution(self):
        fixture = json.loads((ROOT / 'docs/plan/evidence/coverage-unicode-counterexample.json').read_text(encoding='utf-8'))
        self.assertIn('schema_version', fixture)
        self.assertTrue(fixture['original_observation']['passed'])
        self.assertFalse(fixture['original_observation']['correct_attribution_passed'])
        # Populated using the immutable fixture's original bytes, never a diff display.
        files = fixture['before']
        for name, raw in files.items(): self.write(name, raw.encode('utf-8'))
        self.commit(); base = self.git('rev-parse', 'HEAD').decode().strip()
        for name, raw in fixture['after'].items(): self.write(name, raw.encode('utf-8'))
        self.git('add', '.')
        self.git('update-index', '--chmod=+x', 'crates/b/src/lib.rs')
        self.git('commit', '-qm', 'unicode payload and known mode-only change')
        head = self.git('rev-parse', 'HEAD').decode().strip()
        proof = prove(self.root, head=head, base=base, sources=self.sources(*files))
        self.assertEqual(proof['files']['crates/a/src/lib.rs']['added_head_lines'], [3, 23, 24, 25])
        self.assertEqual(proof['files']['crates/b/src/lib.rs']['added_head_lines'], [])

    def test_root_empty_deleted_and_rename_are_explicit(self):
        root_proof = prove(self.root, head=self.base, base=None, sources=self.sources('src/a.rs'))
        self.assertEqual(root_proof['files']['src/a.rs']['added_head_lines'], [1])
        self.git('mv', 'src/a.rs', 'src/new.rs'); self.write('src/empty.rs', b'')
        head = self.commit()
        proof = prove(self.root, head=head, base=self.base, sources=self.sources('src/new.rs', 'src/empty.rs'), deleted=['src/a.rs'])
        self.assertEqual(proof['files']['src/a.rs']['added_head_lines'], [])
        self.assertEqual(proof['files']['src/new.rs']['added_head_lines'], [1])
        self.assertEqual(proof['files']['src/empty.rs']['added_head_lines'], [])
        with self.assertRaisesRegex(Refused, 'parentless_root'): prove(self.root, head=head, base=None, sources={})
        with self.assertRaises(Refused): prove(self.root, head=head, base='all-production', sources={})

    def test_stale_head_manifest_and_actual_mutation_refuse(self):
        sources = self.sources('src/a.rs')
        self.write('src/a.rs', b'changed\n'); head = self.commit()
        with self.assertRaisesRegex(Refused, 'actual_head_mismatch'):
            prove(self.root, head=self.base, base=self.base, sources=sources)
        with self.assertRaisesRegex(Refused, 'manifest_mismatch'):
            prove(self.root, head=head, base=self.base, sources=sources)
        sources = self.sources('src/a.rs'); self.write('src/a.rs', b'changed without commit\n')
        with self.assertRaisesRegex(Refused, 'source_bytes_mismatch'):
            prove(self.root, head=head, base=self.base, sources=sources)

    def test_wrong_missing_nonancestor_and_redirect_views_refuse(self):
        for base in ['0' * 40, 'f' * 40]:
            with self.assertRaises(Refused): prove(self.root, head=self.base, base=base, sources={})
        with patch.dict(os.environ, GIT_DIR=str(self.root / '.git')):
            with self.assertRaisesRegex(Refused, 'redirecting_git_environment'):
                prove(self.root, head=self.base, base=self.base, sources={})

    def test_concurrent_source_bookend_and_corrupt_object_refuse(self):
        self.write('src/z.rs', b'z\n'); head = self.commit()
        sources = self.sources('src/a.rs', 'src/z.rs')
        method = MOD['GitObjects'].raw
        calls = 0
        def mutate(objects, kind, identity, maximum):
            nonlocal calls
            raw = method(objects, kind, identity, maximum)
            if kind == 'blob':
                calls += 1
                if calls == 3: self.write('src/a.rs', b'changed after earlier read\n')
            return raw
        with patch.object(MOD['GitObjects'], 'raw', mutate):
            with self.assertRaisesRegex(Refused, 'frozen_source_changed'):
                prove(self.root, head=head, base=self.base, sources=sources)
        objects = MOD['GitObjects'](self.root, Budget())
        with patch.object(objects, 'command', side_effect=[b'11\n', b'wrong bytes']):
            with self.assertRaisesRegex(Refused, 'object_content_hash_mismatch'):
                objects.raw('blob', '0' * 40, 100)

    def test_unrelated_commit_grafts_alternates_and_replace_refs_refuse(self):
        # An independent root object exists locally but is not an ancestor.
        tree = self.git('rev-parse', self.base + '^{tree}').decode().strip()
        unrelated = self.git('commit-tree', tree, '-m', 'unrelated root').decode().strip()
        with self.assertRaisesRegex(Refused, 'base_is_not_ancestor'):
            prove(self.root, head=self.base, base=unrelated, sources={})
        self.git('replace', self.base, unrelated)
        with self.assertRaisesRegex(Refused, 'replacement_refs'):
            prove(self.root, head=self.base, base=self.base, sources={})
        common = self.root / '.git'
        (common / 'shallow').write_text(self.base + '\n', encoding='ascii')
        with self.assertRaisesRegex(Refused, 'unsupported_git_view'):
            prove(self.root, head=self.base, base=self.base, sources={})

    def test_output_blob_limits_and_source_symlink_refuse(self):
        for limits, reason in [(replace(Limits(), proof_bytes=1), 'proof_output_limit'),
                               (replace(Limits(), blob_bytes=1), 'blob_byte_limit')]:
            with self.assertRaisesRegex(Refused, reason):
                prove(self.root, head=self.base, base=None, sources=self.sources('src/a.rs'), limits=limits)
        with self.assertRaisesRegex(Refused, 'missing_source_path'):
            prove(self.root, head=self.base, base=self.base, sources={'src/missing.rs': manifest(b'', 'fixture')})
        for path in ['../secret', 'a\\b', '/absolute', 'a\nheader', '.git/config']:
            with self.assertRaisesRegex(Refused, 'source_path'):
                prove(self.root, head=self.base, base=self.base, sources={path: manifest(b'', 'fixture')})

    def test_real_child_output_and_deadline_limits_reap_only_owned_child(self):
        objects = MOD['GitObjects'](self.root, Budget())
        with self.assertRaisesRegex(Refused, 'git_output_limit'):
            objects.command('rev-parse', 'HEAD', limit=1)
        popen = subprocess.Popen
        children = []
        def slow_git(*_args, **kwargs):
            child = popen([sys.executable, '-c', 'import time;time.sleep(30)'], **kwargs)
            children.append(child)
            return child
        objects.budget = Budget(replace(Limits(), seconds=1))
        started = time.monotonic()
        with patch.object(MOD['subprocess'], 'Popen', slow_git):
            with self.assertRaisesRegex(Refused, 'proof_deadline'): objects.command('rev-parse', 'HEAD')
        self.assertLess(time.monotonic() - started, 5)
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].poll())


if __name__ == '__main__': unittest.main()
