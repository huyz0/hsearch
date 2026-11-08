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
        native = dict(D['bindings'](self.bundle), schema_version=1, role='native', source=source, target=target, profile=profile,
                      completed=True, cases=copy.deepcopy(cases), native_lucene_revision=self.bundle['fixture']['pins']['native_lucene_revision'],
                      image_id='sha256:' + 'a' * 64, execution_receipt_sha256='4' * 64, build_binding_sha256='5' * 64, test_executor_binding_sha256='6' * 64)
        reference = D['reference_receipt'](self.bundle, source, target, 'sha256:' + '7' * 64,
            {'number': '3.9.0', 'build_hash': self.bundle['fixture']['pins']['reference']['server_revision'], 'lucene_version': '10.5.1'},
            D['file_sha'](ROOT, 'tools/reference.py'), copy.deepcopy(cases))
        trusted = {key: native[key] for key in ('source', 'target', 'profile', 'image_id', 'execution_receipt_sha256', 'build_binding_sha256', 'test_executor_binding_sha256')}
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
        mutations = [lambda n, r: n['source'].update(head='f' * 40), lambda n, r: n.update(build_binding_sha256='e' * 64),
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
