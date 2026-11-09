"""Synthetic HOST validator/controller controls, never actual runtime receipts."""
import copy
import json
from pathlib import Path
import runpy
import shutil
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
D = runpy.run_path(str(ROOT / 'tools/engine_differential.py'))


class FixedReference:
    """Mock ports record request effects; outputs are explicitly synthetic."""
    def __init__(self, bundle, corrupt=False):
        self.bundle, self.corrupt = bundle, corrupt
        self.calls, self.indices = [], {}

    def __call__(self, method, path, body=None, content_type='application/json'):
        self.calls.append((method, path, copy.deepcopy(body), content_type))
        if path.endswith('/_analyze'):
            case = next(x for x in self.bundle['fixture']['analyzer_cases'] if x['text'] == body['text'])
            tokens = [{'token': t['term'], **{k: t[k] for k in ('position', 'start_offset', 'end_offset')}} for t in case['expected']]
            if self.corrupt:
                tokens[0]['position'] = True
            return 200, {'tokens': tokens}
        if method == 'PUT':
            self.indices[path[1:]] = []
            return 200, {}
        if path == '/_bulk?refresh=wait_for':
            rows = [json.loads(line) for line in body.splitlines()]
            items = []
            for metadata, source in zip(rows[::2], rows[1::2]):
                event = metadata['index']
                self.indices[event['_index']].append((event['_id'], source))
                items.append({'index': {'_id': event['_id'], 'status': 201}})
            return 200, {'errors': False, 'items': items}
        if path.endswith('/_search'):
            case = next(x for x in self.bundle['fixture']['search_cases'] + self.bundle['fixture']['discrimination_controls'] if x['reference_request']['body'] == body)
            expected = D['expected_cases'](self.bundle)[case['id']]
            return 200, {'hits': {'hits': [{'_id': value, '_source': {'uid': value}} for value in expected['ids']],
                                  'total': {'value': expected['total'], 'relation': 'eq'}}}
        if method == 'DELETE':
            del self.indices[path[1:]]
            return 200, {}
        raise AssertionError('Unexpected mock request')


class DifferentialControls(unittest.TestCase):
    def setUp(self):
        self.bundle = D['load_fixture'](ROOT)

    def receipts(self, target=D['TARGETS'][0], profile='dev'):
        # These deliberately synthetic records exercise pure validation only.
        source = {'head': '1' * 40, 'tree': '2' * 40, 'source_digest': '3' * 64}
        cases = [{'id': key, 'outcome': 'pass', 'value': value} for key, value in D['expected_cases'](self.bundle).items()]
        native = dict(D['bindings'](self.bundle), schema_version=2, role='native', source=source, target=target, profile=profile,
                      completed=True, cases=copy.deepcopy(cases), native_lucene_revision=self.bundle['fixture']['pins']['native_lucene_revision'],
                      image_id='sha256:' + 'a' * 64, execution_receipt_sha256='4' * 64)
        reference = D['reference_receipt'](self.bundle, source, target, 'sha256:' + '7' * 64,
            {'number': '3.9.0', 'build_hash': self.bundle['fixture']['pins']['reference']['server_revision'], 'lucene_version': '10.5.1'},
            D['file_sha'](ROOT, 'tools/reference.py'), copy.deepcopy(cases))
        trusted = {key: native[key] for key in ('source', 'target', 'profile', 'image_id', 'execution_receipt_sha256')}
        return native, reference, source, trusted

    def test_all_twenty_nine_ids_and_existing_fourteen_four_are_distinct(self):
        expected = D['expected_cases'](self.bundle)
        self.assertEqual(len(expected), 29)
        self.assertEqual(sum(x.startswith('jdiff-v1-') for x in expected), 11)
        self.assertEqual(len(self.bundle['inputs']['fixtures/api/core-v1/mutation-cases.json']), 11)
        self.assertEqual(sum(len(x['steps']) for x in self.bundle['inputs']['fixtures/api/core-v1/mutation-cases.json']), 34)

    def test_owned_reference_supplement_retains_all_eleven_observations(self):
        port = FixedReference(self.bundle)
        result = D['run_reference'](port, self.bundle, 'hsearch-fixture-host-synthetic')
        self.assertEqual(len(result), 11)
        self.assertFalse(port.indices)
        self.assertEqual(sum(call[0] == 'PUT' for call in port.calls), 3)
        self.assertEqual(sum(call[0] == 'DELETE' for call in port.calls), 3)
        self.assertLessEqual(max(len(call[2].encode()) if isinstance(call[2], str) else len(D['canonical'](call[2])) for call in port.calls), 16384)

    def test_boolean_position_cannot_pass_and_no_retry_occurs(self):
        port = FixedReference(self.bundle, corrupt=True)
        with self.assertRaisesRegex(ValueError, 'typed oracle'):
            D['run_reference'](port, self.bundle, 'hsearch-host-synthetic')
        self.assertEqual(len(port.calls), 1)

    def test_unowned_target_and_incomplete_bulk_refuse(self):
        with self.assertRaisesRegex(ValueError, 'Owned reference'):
            D['run_reference'](FixedReference(self.bundle), self.bundle, 'http://external')
        port = FixedReference(self.bundle)
        def incomplete(method, path, body=None, content_type='application/json'):
            status, value = port(method, path, body, content_type)
            if path == '/_bulk?refresh=wait_for':
                value['items'].pop()
            return status, value
        with self.assertRaisesRegex(ValueError, 'load incomplete'):
            D['run_reference'](incomplete, self.bundle, 'hsearch-host-synthetic')
        self.assertFalse(any(x[1].endswith('/_search') for x in port.calls))

    def test_synthetic_join_matrix_profiles_target_bindings(self):
        for target in D['TARGETS']:
            for profile in ('dev', 'release'):
                native, reference, source, trusted = self.receipts(target, profile)
                result = D['validate_join'](native, reference, source, trusted, ROOT)
                self.assertEqual(result['join'], 'consistent')
                self.assertEqual((result['target'], result['profile']), (target, profile))
                self.assertEqual(len(result['case_ids']), 29)
                self.assertIn('not caller authentication', result['scope'])

    def test_stale_build_image_source_generator_and_profile_cannot_join(self):
        mutations = [lambda n, r: n['source'].update(head='f' * 40), lambda n, r: n.update(execution_receipt_sha256='e' * 64),
                     lambda n, r: n.update(image_id='sha256:' + 'b' * 64), lambda n, r: n.update(profile='release'),
                     lambda n, r: n.update(generator_sha256='c' * 64), lambda n, r: r.update(controller_sha256='d' * 64),
                     lambda n, r: r.update(pinned_image='sha256:' + 'e' * 64), lambda n, r: r['server_version'].update(lucene_version='10.5.0')]
        for edit in mutations:
            native, reference, source, trusted = self.receipts()
            source, trusted = copy.deepcopy(source), copy.deepcopy(trusted)
            edit(native, reference)
            with self.assertRaises(ValueError):
                D['validate_join'](native, reference, source, trusted, ROOT)

    def test_complete_matrix_refuses_missing_profiles_and_duplicate_cells(self):
        natives, references, bindings = [], [], []
        for target in D['TARGETS']:
            for profile in ('dev', 'release'):
                native, reference, source, trusted = self.receipts(target, profile)
                natives.append(native)
                bindings.append(trusted)
                if profile == 'dev':
                    references.append(reference)
        result = D['validate_matrix'](natives, references, source, bindings, ROOT)
        self.assertEqual(len(result['pairs']), 4)
        for ns, rs in ((natives[:-1], references), (natives, references[:-1]), (natives[:-1] + [natives[0]], references)):
            with self.assertRaises(ValueError):
                D['validate_matrix'](ns, rs, source, bindings, ROOT)

    def test_metric_numeric_equivalence_does_not_weaken_counts_or_tokens(self):
        native, reference, source, trusted = self.receipts()
        metric = next(x for x in native['cases'] if x['id'] == 'finite-metrics')
        metric['value']['values']['min_price'] = -0.0
        metric['value']['values']['sum_price'] = 240
        self.assertEqual(D['validate_join'](native, reference, source, trusted, ROOT)['join'], 'consistent')
        metric['value']['values']['min_price'] = False
        with self.assertRaises(ValueError):
            D['validate_join'](native, reference, source, trusted, ROOT)

    def test_missing_duplicate_skipped_boolean_and_changed_values_refuse(self):
        edits = [lambda x: x['cases'].pop(), lambda x: x['cases'].__setitem__(1, copy.deepcopy(x['cases'][0])),
                 lambda x: x['cases'][0].update(outcome='skipped'), lambda x: x.update(completed=1),
                 lambda x: x['cases'][0]['value'].update(total=True), lambda x: x.update(unreviewed_extra=True)]
        for edit in edits:
            native, reference, source, trusted = self.receipts()
            edit(native)
            with self.assertRaises(ValueError):
                D['validate_join'](native, reference, source, trusted, ROOT)

    def test_float_or_boolean_UTF16_fields_cannot_be_accepted(self):
        for bad in (True, 0.0):
            case = self.bundle['fixture']['analyzer_cases'][0]
            token = case['expected'][0]
            body = {'tokens': [{'token': token['term'], 'position': bad, 'start_offset': 0, 'end_offset': 255}]}
            with self.assertRaises(ValueError):
                D['observation'](self.bundle, case, 200, body)

    def test_hash_stale_or_duplicate_JSON_and_size_depth_limits(self):
        with self.assertRaises(ValueError):
            D['decode'](b'{"a":1,"a":2}')
        for raw in (b'{' + b' ' * 65536 + b'}', b'{"x":NaN}', b'[' * 26 + b'0' + b']' * 26):
            with self.assertRaises(ValueError):
                D['decode'](raw)
        with tempfile.TemporaryDirectory() as area:
            root = Path(area)
            for name in (D['FIXTURE'], D['PROVENANCE']):
                target = root / name
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / name, target)
            (root / D['FIXTURE']).write_bytes((root / D['FIXTURE']).read_bytes() + b' ')
            with self.assertRaisesRegex(ValueError, 'hash mismatch'):
                D['load_fixture'](root)

    def test_integer_date_wire_boundaries_and_frozen_discriminators(self):
        held = self.bundle['inputs']['fixtures/engine-research/held-out.json']
        self.assertEqual(D['date_wire'](held['date']['minimum']), '2026-01-01T00:00:00.000Z')
        self.assertEqual(D['date_wire'](held['date']['maximum']), '2026-01-01T00:00:00.002Z')
        with self.assertRaises(ValueError):
            D['date_wire'](True)
        expected = D['expected_cases'](self.bundle)
        self.assertNotEqual(expected['jdiff-v1-held-out-phrase'], expected['jdiff-v1-held-out-phrase-unordered-control'])
        self.assertNotEqual(expected['jdiff-v1-held-out-date'], expected['jdiff-v1-held-out-date-all-control'])

    def test_generated_observations_cover_all_case_ids_and_actual_result_fields(self):
        generator = runpy.run_path(str(ROOT / 'crates/engine/tests/render_adapter_fixtures.py'))
        source = generator['render_supplement'](ROOT)
        self.assertEqual(source.count('#[test]'), 29)
        for identity in D['expected_cases'](self.bundle):
            self.assertIn('fn differential_' + identity.replace('-', '_') + '()', source)
        self.assertIn('t.position,t.start_offset,t.end_offset', source)
        self.assertIn('result.hits.iter()', source)
        self.assertIn('m.sum,m.min.unwrap(),m.max.unwrap(),m.count', source)
        self.assertNotIn('CARGO_MANIFEST_DIR', source)  # No guessed execution-head receipt.


if __name__ == '__main__':
    unittest.main()


def synthetic_capture(target=D['TARGETS'][0]):
    """Only synthetic HOST records; these hashes are never runtime evidence."""
    import hashlib
    sandbox = runpy.run_path(str(ROOT / 'tools/sandbox.py'))
    bundle = D['load_fixture'](ROOT)
    policy = D['read_json'](ROOT, 'dev/correctness-tools.json')
    arch = target.split('-')[0]
    version = 'cargo-nextest ' + policy['nextest']['version'] + '\nhost: synthetic'
    tools = {key: '/synthetic/' + key for key in D['TOOL_KEYS']}
    tools.update(rustc='rustc 1.97.1 synthetic', cargo='cargo 1.97.1 synthetic', native_target=target,
        cargo_compiler='/qualified/bin/rustc', cargo_rustdoc='/qualified/bin/rustdoc', nextest_executable='/qualified/nextest',
        nextest=version, python_executable='/usr/bin/python3', hashes={k: 'a' * 64 for k in
        ('rustc', 'rustdoc', 'cargo', 'cargo_clippy', 'clippy_driver', 'cargo_fmt', 'rustfmt', 'llvm_cov', 'llvm_profdata', 'llvm_objdump', 'nextest')},
        compiler_policy_sha256=D['file_sha'](ROOT, 'harness/cargo-correctness.toml'),
        compiler_wrappers={'RUSTC_WRAPPER': '', 'RUSTC_WORKSPACE_WRAPPER': ''},
        compiler_environment={'RUSTC_BOOTSTRAP': '-1', 'CLIPPY_ARGS': '-D__CLIPPY_HACKERY__warnings__CLIPPY_HACKERY__'},
        test_executor={'path': '/usr/bin/env', 'command': '/usr/bin/env --', 'sha256': 'b' * 64,
                       'environment_key': 'CARGO_TARGET_' + target.upper().replace('-', '_') + '_RUNNER'})
    image = {'id': 'sha256:' + 'c' * 64, 'architecture': arch, 'engine': 'synthetic-engine',
        'build_inputs_sha256': sandbox['image_digest'](), 'dockerfile_sha256': D['file_sha'](ROOT, 'dev/Dockerfile'),
        'correctness_artifact': {'architecture': arch, 'revision': policy['nextest']['revision'],
        'archive_sha256': policy['nextest']['architectures'][arch]['archive_sha256'], 'profile_sha256': D['digest'](policy),
        'binary_sha256': tools['hashes']['nextest'], 'version': version}}
    proof = {'container_id': 'd' * 64, 'image_id': image['id'], 'architecture': arch, 'engine': image['engine'],
        'user': '10001:10001', 'read_only': True, 'network': 'none', 'limits': {'cpu_nanos': 2_000_000_000,
        'memory': 4 * 1024**3, 'memory_swap': 4 * 1024**3, 'pids': 256},
        'state': {'status': 'exited', 'running': False, 'exit_code': 0}, 'budget': sandbox['BUILD'], 'output_budget': 512 * 1024}
    source = {'head': '1' * 40, 'tree': '2' * 40, 'source_digest': '3' * 64}
    lines = [json.dumps({'engine_differential_tools': tools})]
    commands = D['qualified_commands'](tools, ROOT)
    expected = D['expected_cases'](bundle)
    for command in commands:
        profile = 'release' if '--release' in command else 'dev'
        capture = 'adapter_java_supplement' in command
        if capture:
            lines.append(json.dumps({'engine_differential_capture': {'profile': profile, 'state': 'started'}}))
        lines.append('CHECK: ' + command)
        if capture:
            for identity, value in expected.items():
                lines.append(json.dumps({'engine_differential_native_case': {'id': identity, 'outcome': 'pass', 'fixture_sha256': D['FIXTURE_SHA'], 'value': value}}))
            lines.append(json.dumps({'engine_differential_capture': {'profile': profile, 'state': 'completed'}}))
    lines.append('PASS: complete bootstrap suite; remaining milestone/runtime gates are recorded in the ledger')
    monitor = {'mode': 'check', 'exit': 0, 'elapsed_seconds': 500.0, 'peak_disk_bytes': 6 * 1024**3, 'output_bytes': 0}
    # The guest counts child bytes before printing its own terminal monitor.
    monitor['output_bytes'] = len(('\n'.join(lines) + '\n').encode())
    lines.append(json.dumps(monitor))
    result = {'mode': 'check', 'container_id': proof['container_id'], 'exit_code': 0, 'truncated': False,
              'encoding': 'utf-8-replacement', 'execution': proof, 'output': '\n'.join(lines) + '\n'}
    reference = D['reference_receipt'](bundle, source, target, 'sha256:' + '7' * 64,
        {'number': '3.9.0', 'build_hash': bundle['fixture']['pins']['reference']['server_revision'], 'lucene_version': '10.5.1'},
        D['file_sha'](ROOT, 'tools/reference.py'), [{'id': i, 'outcome': 'pass', 'value': v} for i, v in expected.items()])
    return result, source, image, reference


class NativeCollectorControls(unittest.TestCase):
    def test_complete_actual_shape_mock_matrix_and_observed_values(self):
        packages = []
        for target in D['TARGETS']:
            result, source, image, reference = synthetic_capture(target)
            capture = D['collect_native'](result, source, copy.deepcopy(source), image, ROOT)
            self.assertEqual([n['schema_version'] for n in capture['native']], [2, 2])
            packages.append(D['architecture_package'](capture, reference))
            self.assertEqual(capture['manifest']['commands']['count'], len(D['qualified_commands'](capture['manifest']['tools'], ROOT)))
        self.assertLessEqual(len(D['canonical'](packages)), D['CAP'])
        self.assertEqual(len(D['package_matrix'](packages, source, ROOT)['pairs']), 4)
        # A wrong observed value must fail, even when the outer process passed.
        result['output'] = result['output'].replace('"total": 1', '"total": 2', 1)
        with self.assertRaises(ValueError):
            D['collect_native'](result, source, source, image, ROOT)

    def test_native_v1_cannot_pass_prospective_v2(self):
        result, source, image, reference = synthetic_capture()
        capture = D['collect_native'](result, source, source, image, ROOT)
        native = capture['native'][0]
        native.update(schema_version=1, build_binding_sha256='8' * 64, test_executor_binding_sha256='9' * 64)
        with self.assertRaises(ValueError):
            D['verify_package'](D['architecture_package'](capture, reference), source, ROOT)

    def test_failed_truncated_dirty_foreign_image_arch_tool_and_executor(self):
        mutations = [lambda r, s, i: r.update(exit_code=1), lambda r, s, i: r.update(truncated=True),
            lambda r, s, i: r['execution'].update(image_id='sha256:' + '0' * 64),
            lambda r, s, i: r['execution'].update(architecture='aarch64'),
            lambda r, s, i: r['execution']['limits'].update(memory_swap=0),
            lambda r, s, i: r['execution'].update(container_id='bad'),
            lambda r, s, i: r.update(output=r['output'].replace('/usr/bin/env --', '/bin/false')),
            lambda r, s, i: r.update(output=r['output'].replace('rustc 1.97.1', 'rustc 1.96.0')),
            lambda r, s, i: i.update(build_inputs_sha256='e' * 64),
            lambda r, s, i: r.update(output=r['output'] + 'unexpected post-terminal output\n')]
        for mutate in mutations:
            result, source, image, _ = synthetic_capture()
            mutate(result, source, image)
            with self.subTest(mutation=mutate), self.assertRaises(ValueError):
                D['collect_native'](result, source, source, image, ROOT)
        result, source, image, _ = synthetic_capture()
        with self.assertRaises(ValueError):
            D['collect_native'](result, source, dict(source, head='f' * 40), image, ROOT)

    def test_missing_duplicate_malformed_unsegmented_and_crossprofile_events(self):
        result, source, image, _ = synthetic_capture()
        lines = result['output'].splitlines()
        index = next(i for i, line in enumerate(lines) if 'engine_differential_native_case' in line)
        marker = next(i for i, line in enumerate(lines) if '"state": "started"' in line)
        variants = [lines[:index] + lines[index + 1:], lines[:index] + [lines[index]] + lines[index:],
            lines[:index] + ['{"engine_differential_native_case":'] + lines[index + 1:],
            [lines[index]] + lines, lines[:marker] + [lines[marker].replace('dev', 'release')] + lines[marker + 1:],
            [line for line in lines if '"state": "completed"' not in line],
            [line for line in lines if 'CHECK: ' not in line],
            [line.replace(D['FIXTURE_SHA'], '0' * 64) if 'engine_differential_native_case' in line else line for line in lines]]
        for variant in variants:
            with self.subTest(length=len(variant)), self.assertRaises(ValueError):
                D['collect_native'](dict(result, output='\n'.join(variant) + '\n'), source, source, image, ROOT)
        for invalid in ('x' * (512 * 1024 + 1), '{"engine_differential_tools":' + '[' * 26 + '0' + ']' * 26 + '}'):
            with self.assertRaises(ValueError):
                D['collect_native'](dict(result, output=invalid), source, source, image, ROOT)

    def test_transport_proofs_and_cases_cannot_be_replaced_by_hash_only(self):
        result, source, image, reference = synthetic_capture()
        package = D['architecture_package'](D['collect_native'](result, source, source, image, ROOT), reference)
        variants = [dict(package, manifest={'sha256': 'a' * 64}), dict(package, native=package['native'][:1]),
                    dict(package, reference=dict(reference, source=dict(source, head='f' * 40)))]
        for bad in variants:
            with self.assertRaises(ValueError):
                D['verify_package'](bad, source, ROOT)
        package['native'][0]['cases'][0]['value'] = {'kind': 'hits', 'ids': [], 'total': 0, 'projection': []}
        with self.assertRaises(ValueError):
            D['verify_package'](package, source, ROOT)

    def test_public_paths_refuse_source_private_reuse_and_symlink(self):
        with tempfile.TemporaryDirectory() as area:
            private = Path(area) / 'private'
            private.mkdir()
            for path in (ROOT / 'bad-output', private / 'bad-output', private):
                with self.assertRaises(ValueError):
                    D['evidence_directory'](path, ROOT, private)
            good = D['evidence_directory'](Path(area) / 'public', ROOT, private)
            D['save_public'](good / 'ok.json', {'public': True})
            with self.assertRaises(FileExistsError):
                D['save_public'](good / 'ok.json', {'public': False})
            with self.assertRaises(ValueError):
                D['save_public'](good / 'big.json', {'value': 'x' * 65536})


class TransportAndPackagingControls(unittest.TestCase):
    def test_real_length_tool_identity_matrix_remains_bounded(self):
        packages = []
        for target in D['TARGETS']:
            result, source, image, reference = synthetic_capture(target)
            lines = result['output'].splitlines()
            event = json.loads(lines[0])
            tools = event['engine_differential_tools']
            tools['rustc'] = 'rustc 1.97.1 (synthetic)\nbinary: rustc\ncommit-hash: ' + 'a' * 40 + '\ncommit-date: 2026-07-14\nhost: ' + target + '\nrelease: 1.97.1\nLLVM version: 22.1.0'
            tools['cargo'] = 'cargo 1.97.1 (synthetic 2026-06-30)'
            tools['nextest'] = 'cargo-nextest 0.9.150 (synthetic)\nrelease: 0.9.150\ncommit-hash: ' + 'b' * 40 + '\ncommit-date: 2026-10-26\nhost: ' + target.replace('-gnu', '-musl')
            image['correctness_artifact']['version'] = tools['nextest']
            lines[0] = json.dumps(event)
            result['output'] = '\n'.join(lines) + '\n'
            capture = D['collect_native'](result, source, source, image, ROOT)
            packages.append(D['architecture_package'](capture, reference))
        self.assertLessEqual(len(D['canonical'](packages)), D['CAP'])
        self.assertEqual(len(D['package_matrix'](packages, source, ROOT)['pairs']), 4)
        packages[0]['bindings']['module_sha256'] = 'f' * 64
        with self.assertRaises(ValueError): D['package_matrix'](packages, source, ROOT)

    def test_package_cli_only_reads_retained_public_receipts(self):
        import os
        import sys
        from unittest.mock import patch
        result, source, image, reference = synthetic_capture()
        capture = D['collect_native'](result, source, source, image, ROOT)
        with tempfile.TemporaryDirectory() as area:
            native = Path(area) / 'native'
            native.mkdir()
            D['save_public'](native / 'capture.json', capture)
            refs = Path(area) / 'reference'
            owned = refs / 'owned'
            owned.mkdir(parents=True)
            (owned / 'observations.json').write_text(json.dumps({'engine_differential': reference, 'other_public_observations': ['toy']}), encoding='utf-8')
            output = Path(area) / 'github-output'
            with patch.dict(D['package_cli'].__globals__, source_identity=lambda: source), patch.object(sys, 'argv', ['package', '--native', str(native), '--reference-parent', str(refs)]), patch.dict(os.environ, GITHUB_OUTPUT=str(output)):
                D['package_cli']()
            package = D['decode']((native / 'package.json').read_bytes())
            self.assertEqual(len(D['verify_package'](package, source, ROOT)), 2)
            self.assertTrue(output.read_text(encoding='utf-8').startswith('differential={'))
