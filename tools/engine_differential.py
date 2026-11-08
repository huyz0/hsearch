"""Fixed hash-bound engine cases and pure observation joining; no execution authority."""
from datetime import datetime, timezone, timedelta
import hashlib
import json
import math
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
CAP = 65536
FIXTURE = 'fixtures/engine-research/java-differential-v1.json'
PROVENANCE = 'fixtures/engine-research/java-differential-v1.PROVENANCE.json'
FIXTURE_SHA = 'e56f8bf8ee611a69998bdd1961081a52e8830fd71a9346aeea53d9980c719b2e'
PROVENANCE_SHA = '4ee3121ce1097663374ebff7a531e99fb676b8a21d469f385246392da4202734'
TARGETS = ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')
SHA = re.compile(r'[0-9a-f]{64}')


def need(condition, reason):
    if not condition:
        raise ValueError(reason)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False, allow_nan=False).encode()


def digest(value):
    return hashlib.sha256(canonical(value)).hexdigest()


def file_sha(root, name):
    path = Path(root) / name
    need(not path.is_symlink() and path.is_file() and path.stat().st_size <= 1024**2, 'Bounded source file required')
    return hashlib.sha256(path.read_bytes()).hexdigest()


def bounded(value):
    # Iterative structural check precedes serialization of externally supplied objects.
    pending, visits = [(value, 0)], 0
    while pending:
        item, depth = pending.pop()
        visits += 1
        need(visits <= 50000 and depth <= 24, 'Differential structure bound')
        if type(item) is dict:
            need(len(item) <= 4096 and all(type(key) is str and len(key.encode()) <= 4096 for key in item), 'Differential object bound')
            pending.extend((child, depth + 1) for child in item.values())
        elif type(item) is list:
            need(len(item) <= 4096, 'Differential sequence bound')
            pending.extend((child, depth + 1) for child in item)
        elif type(item) is str:
            need(len(item.encode()) <= 8192, 'Differential string bound')
        else:
            need(item is None or type(item) in {bool, int, float}, 'Differential JSON type')
            need(type(item) is not int or -(2**63) <= item < 2**64, 'Differential integer bound')
            need(type(item) is not float or math.isfinite(item), 'Nonfinite differential number')
    need(len(canonical(value)) <= CAP, 'Differential byte bound')
    return value


def decode(raw):
    need(type(raw) is bytes and 0 < len(raw) <= CAP, 'Bounded differential JSON required')
    def pairs(rows):
        result = {}
        for key, value in rows:
            need(key not in result, 'Duplicate differential JSON field')
            result[key] = value
        return result
    try:
        return bounded(json.loads(raw, object_pairs_hook=pairs, parse_constant=lambda _: (_ for _ in ()).throw(ValueError('Nonfinite differential JSON'))))
    except (RecursionError, UnicodeError) as error:
        raise ValueError('Invalid bounded differential JSON') from error


def read_json(root, name, pin=None):
    path = Path(root) / name
    need(path.resolve() == path.absolute() and not path.is_symlink() and path.is_file() and path.stat().st_size <= CAP, 'Bounded regular fixture required')
    raw = path.read_bytes()
    need(pin is None or hashlib.sha256(raw).hexdigest() == pin, 'Differential fixture hash mismatch')
    return decode(raw)


def load_fixture(root=ROOT):
    fixture = read_json(root, FIXTURE, FIXTURE_SHA)
    provenance = read_json(root, PROVENANCE, PROVENANCE_SHA)
    need(type(fixture['schema_version']) is int and fixture['schema_version'] == 1 and fixture['format'] == 'hsearch-engine-java-differential-v1', 'Differential fixture version')
    need(provenance['fixture_sha256'] == FIXTURE_SHA, 'Differential provenance binding')
    inputs = {}
    for item in fixture['immutable_inputs']:
        need(item['path'] not in inputs and item['path'].startswith(('fixtures/api/core-v1/', 'fixtures/engine-research/')), 'Fixed immutable input path')
        inputs[item['path']] = read_json(root, item['path'], item['sha256'])
    need(len(inputs) == 7, 'Complete immutable fixture inventory')
    manifest = read_json(root, 'fixtures/api/core-v1/manifest.json')
    need(fixture['pins']['reference'] == manifest['reference'], 'Pinned reference mismatch')
    need({x['path']: x['sha256'] for x in fixture['immutable_inputs'] if '/core-v1/' in x['path']} == {'fixtures/api/core-v1/' + k: v for k, v in manifest['files'].items()}, 'Original fixture pins mismatch')
    ids = [x['id'] for key in ('analyzer_cases', 'search_cases', 'discrimination_controls') for x in fixture[key]]
    need(len(ids) == len(set(ids)) == 11 and all(re.fullmatch(r'jdiff-v1-[a-z0-9-]{1,63}', x) for x in ids), 'Supplement IDs incomplete')
    need([len(fixture[key]) for key in ('analyzer_cases', 'search_cases', 'discrimination_controls')] == [6, 3, 2], 'Supplement family counts')
    return {'fixture': fixture, 'provenance': provenance, 'inputs': inputs, 'root': Path(root)}


def resolve(bundle, ref):
    need(ref['path'] == 'fixtures/engine-research/held-out.json' and ref['sha256'] == next(x['sha256'] for x in bundle['fixture']['immutable_inputs'] if x['path'] == ref['path']), 'Fixed held-out binding required')
    value = bundle['inputs'][ref['path']]
    need(ref['pointer'].startswith('/') and '~' not in ref['pointer'], 'Fixed held-out pointer required')
    for part in ref['pointer'][1:].split('/'):
        need(type(value) is dict and part in value, 'Missing held-out pointer')
        value = value[part]
    need(type(value) is list, 'Held-out sequence required')
    projection = ref.get('projection')
    need(projection in {None, 'identity', 'uid ascending'}, 'Unknown held-out projection')
    return sorted(value) if projection == 'uid ascending' else value


def records(bundle, case):
    return case['documents'] if 'documents' in case else resolve(bundle, case['documents_ref'])


def expected_hits(ids, total):
    return {'kind': 'hits', 'ids': ids, 'total': total, 'projection': [{'uid': identity} for identity in ids]}


def original_value(case):
    if 'tokens' in case:
        return {'kind': 'tokens', 'tokens': [{'term': t['token'], **{k: t[k] for k in ('position', 'start_offset', 'end_offset')}} for t in case['tokens']]}
    oracle = case['reference_oracle']
    if oracle['kind'] == 'hits':
        return expected_hits(oracle['ids'], oracle['exact_total'])
    values = {name: value if type(value) is list else float(value) for name, value in oracle['values'].items()}
    return {'kind': 'aggregations', 'values': values}


def normalize_metrics(value, expected):
    """Only metric scalars use the existing finite numeric equivalence rule."""
    if expected.get('kind') != 'aggregations':
        return value
    need(type(value) is dict and set(value) == {'kind', 'values'} and value['kind'] == 'aggregations', 'Metric projection shape')
    need(type(value['values']) is dict and set(value['values']) == set(expected['values']), 'Metric projection names')
    result = {'kind': 'aggregations', 'values': dict(value['values'])}
    for name, oracle in expected['values'].items():
        if type(oracle) is not list:
            observed = value['values'][name]
            need(type(observed) in {int, float} and math.isfinite(observed), 'Finite numeric metric required')
            result['values'][name] = 0.0 if observed == 0 else float(observed)
    return result


def expected_cases(bundle):
    result = {}
    for name in ('search-cases.json', 'analyzer-cases.json'):
        for case in bundle['inputs']['fixtures/api/core-v1/' + name]:
            need(case['id'] not in result, 'Duplicate original ID')
            result[case['id']] = original_value(case)
    need(len(result) == 18, 'Original eighteen cases required')
    for case in bundle['fixture']['analyzer_cases']:
        result[case['id']] = {'kind': 'tokens', 'tokens': case['expected']}
    for case in bundle['fixture']['search_cases'] + bundle['fixture']['discrimination_controls']:
        ids = case['expected_ids'] if 'expected_ids' in case else resolve(bundle, case['expected_ids_ref'])
        result[case['id']] = expected_hits(ids, case['expected_total'])
    need(len(result) == 29, 'Complete original and supplement cases required')
    return result


def project(case, body):
    if 'tokens' in case or 'text' in case and 'expected' in case:
        return {'kind': 'tokens', 'tokens': [{'term': t['token'], **{k: t[k] for k in ('position', 'start_offset', 'end_offset')}} for t in body['tokens']]}
    if 'reference_oracle' not in case or case['reference_oracle']['kind'] == 'hits':
        hits = body['hits']
        need(hits['total']['relation'] == 'eq', 'Exact differential total required')
        return {'kind': 'hits', 'ids': [h['_id'] for h in hits['hits']], 'total': hits['total']['value'], 'projection': [h['_source'] for h in hits['hits']]}
    values = {}
    for name, expected in case['reference_oracle']['values'].items():
        value = body['aggregations'][name]
        if type(expected) is list:
            values[name] = [{k: item[k] for k in ('key', 'doc_count')} for item in value['buckets']]
        else:
            need(type(value['value']) in {int, float} and math.isfinite(value['value']), 'Finite numeric differential metric required')
            values[name] = 0.0 if value['value'] == 0 else float(value['value'])
    return {'kind': 'aggregations', 'values': values}


def observation(bundle, case, status, body):
    need(type(status) is int and status == 200, 'Differential HTTP failure')
    value = project(case, body)
    need(canonical(value) == canonical(expected_cases(bundle)[case['id']]), 'Differential typed oracle mismatch: ' + case['id'])
    return {'id': case['id'], 'outcome': 'pass', 'value': value}


def date_wire(milliseconds):
    need(type(milliseconds) is int, 'Integer epoch milliseconds required')
    value = datetime(1970, 1, 1, tzinfo=timezone.utc) + timedelta(milliseconds=milliseconds)
    return value.isoformat(timespec='milliseconds').replace('+00:00', 'Z')


def run_reference(request, bundle, owned_index):
    """Only callbacks from the existing owned reference controller perform effects."""
    need(re.fullmatch(r'[a-z0-9-]{1,100}', owned_index) is not None, 'Owned reference index required')
    fixture, output = bundle['fixture'], []
    def call(method, path, body=None, content_type='application/json'):
        wire = body.encode() if type(body) is str else canonical(body) if body is not None else b''
        need(len(wire) <= 16384, 'Supplement request bound')
        return request(method, path, body, content_type)
    for case in fixture['analyzer_cases']:
        status, body = call('POST', '/' + owned_index + '/_analyze', case['reference_request']['body'])
        output.append(observation(bundle, case, status, body))
    for number, case in enumerate(fixture['search_cases']):
        index = owned_index + '-jdiff-' + str(number)
        schema = {'settings': fixture['mapping']['reference_settings'], 'mappings': {'dynamic': 'strict', 'properties': fixture['mapping']['reference_properties']}}
        status, _ = call('PUT', '/' + index, schema)
        need(type(status) is int and status == 200, 'Supplement index creation failed')
        documents, lines = records(bundle, case), []
        need(0 < len(documents) <= 128, 'Supplement document bound')
        for row in documents:
            source = {'uid': row['id']}
            source.update({'title': row['title']} if 'title' in row else {'date': date_wire(row['epoch_ms'])})
            need(len(source) <= 16 and len(canonical(source)) <= 8192, 'Supplement field/document bound')
            lines.extend([canonical({'index': {'_index': index, '_id': row['id']}}).decode(), canonical(source).decode()])
        status, body = call('POST', '/_bulk?refresh=wait_for', '\n'.join(lines) + '\n', 'application/x-ndjson')
        need(type(status) is int and status == 200 and body['errors'] is False and len(body['items']) == len(documents), 'Supplement load incomplete')
        for row, item in zip(documents, body['items']):
            event = item['index']
            need(event['_id'] == row['id'] and type(event['status']) is int and 200 <= event['status'] < 300 and 'error' not in event, 'Supplement item load failed')
        for query in [case] + [x for x in fixture['discrimination_controls'] if x['dataset'] == case['id']]:
            status, body = call('POST', '/' + index + '/_search', query['reference_request']['body'])
            output.append(observation(bundle, query, status, body))
        status, _ = call('DELETE', '/' + index)
        need(type(status) is int and status == 200, 'Supplement index deletion failed')
    need(len(output) == 11 and len({x['id'] for x in output}) == 11, 'Incomplete reference supplement')
    return bounded(output)


def bindings(bundle):
    root = bundle['root']
    return {'fixture_sha256': FIXTURE_SHA, 'provenance_sha256': PROVENANCE_SHA,
            'fixture_file_sha256': {x['path']: x['sha256'] for x in bundle['fixture']['immutable_inputs']},
            'oracle_sha256': digest(expected_cases(bundle)),
            'generator_sha256': file_sha(root, 'crates/engine/tests/render_adapter_fixtures.py'),
            'native_tests_sha256': file_sha(root, 'crates/engine/tests/adapter_java_supplement.rs'),
            'module_sha256': file_sha(root, 'tools/engine_differential.py')}


def reference_receipt(bundle, source, target, image_id, server_version, controller_sha256, cases):
    value = dict(bindings(bundle), schema_version=1, role='reference', source=source, target=target, profile='reference', completed=True,
                 pinned_image=bundle['fixture']['pins']['reference']['linux_amd64' if target == TARGETS[0] else 'linux_arm64'],
                 image_id=image_id, server_version={k: server_version[k] for k in ('number', 'build_hash', 'lucene_version')},
                 controller_sha256=controller_sha256, cases=cases)
    return bounded(value)


def validate_join(native, reference, expected_source, expected_native_bindings, root=ROOT):
    """Validate supplied real receipts; this pure function cannot authenticate a caller."""
    bounded({'native': native, 'reference': reference, 'expected_source': expected_source, 'expected_native_bindings': expected_native_bindings})
    bundle = load_fixture(root)
    need(set(expected_source) == {'head', 'tree', 'source_digest'} and all(re.fullmatch(r'[0-9a-f]{40}' if k != 'source_digest' else r'[0-9a-f]{64}', v) for k, v in expected_source.items()), 'Exact trusted source required')
    common = set(bindings(bundle)) | {'schema_version', 'role', 'source', 'target', 'profile', 'completed', 'cases'}
    execution_keys = {'execution_receipt_sha256', 'build_binding_sha256', 'test_executor_binding_sha256'}
    need(set(native) == common | {'native_lucene_revision', 'image_id'} | execution_keys, 'Closed native receipt schema')
    need(set(reference) == common | {'pinned_image', 'image_id', 'server_version', 'controller_sha256'}, 'Closed reference receipt schema')
    for value, role in ((native, 'native'), (reference, 'reference')):
        need(type(value['schema_version']) is int and value['schema_version'] == 1 and value['role'] == role and value['completed'] is True, 'Incomplete/wrong differential receipt')
        need(value['source'] == expected_source and value['target'] in TARGETS, 'Stale source or unknown target')
        for key, expected in bindings(bundle).items():
            need(value[key] == expected, 'Differential source/input binding mismatch: ' + key)
        cases = value['cases']
        need(type(cases) is list and len(cases) == 29 and len({x['id'] for x in cases}) == 29, 'Missing/duplicate differential case')
        expected = expected_cases(bundle)
        need({x['id'] for x in cases} == set(expected), 'Wrong differential case IDs')
        for case in cases:
            need(set(case) == {'id', 'outcome', 'value'} and case['outcome'] == 'pass', 'Skipped/failed differential case')
            need(canonical(normalize_metrics(case['value'], expected[case['id']])) == canonical(expected[case['id']]), 'Differential value/oracle mismatch')
    need(native['target'] == reference['target'] and native['profile'] in {'dev', 'release'} and reference['profile'] == 'reference', 'Differential matrix mismatch')
    need(native['native_lucene_revision'] == bundle['fixture']['pins']['native_lucene_revision'], 'Native revision mismatch')
    need(set(expected_native_bindings) == {'source', 'target', 'profile', 'image_id'} | execution_keys, 'Trusted native bindings required')
    need(expected_native_bindings['source'] == expected_source, 'Trusted native source mismatch')
    for key in ('source', 'target', 'profile', 'image_id', *sorted(execution_keys)):
        need(native[key] == expected_native_bindings[key], 'Native execution binding mismatch: ' + key)
    need(type(native['image_id']) is str and re.fullmatch(r'sha256:[0-9a-f]{64}', native['image_id']), 'Actual native image required')
    for key in execution_keys:
        need(type(native[key]) is str and SHA.fullmatch(native[key]) is not None and native[key] != '0' * 64, 'Actual native execution bindings required')
    target_key = 'linux_amd64' if native['target'] == TARGETS[0] else 'linux_arm64'
    need(reference['pinned_image'] == bundle['fixture']['pins']['reference'][target_key] and re.fullmatch(r'sha256:[0-9a-f]{64}', reference['image_id']), 'Actual reference image mismatch')
    need(reference['server_version'] == {'number': '3.9.0', 'build_hash': bundle['fixture']['pins']['reference']['server_revision'], 'lucene_version': '10.5.1'}, 'Actual reference build mismatch')
    need(reference['controller_sha256'] == file_sha(root, 'tools/reference.py'), 'Reference controller mismatch')
    return bounded({'schema_version': 1, 'join': 'consistent', 'source': expected_source, 'target': native['target'], 'profile': native['profile'],
                    'case_ids': sorted(expected_cases(bundle)), 'native_receipt_sha256': digest(native), 'reference_receipt_sha256': digest(reference),
                    'scope': 'Trusted supplied execution receipts only; not caller authentication, full matrix, authority, durability or task acceptance.'})


def validate_matrix(native, reference, expected_source, expected_native_bindings, root=ROOT):
    """Require all four native cells and both independent Java architecture receipts."""
    bounded({'native': native, 'reference': reference, 'source': expected_source, 'bindings': expected_native_bindings})
    need(type(native) is list and len(native) == 4 and type(reference) is list and len(reference) == 2 and type(expected_native_bindings) is list and len(expected_native_bindings) == 4, 'Complete differential matrix required')
    cells = {(target, profile) for target in TARGETS for profile in ('dev', 'release')}
    keys = [(item['target'], item['profile']) for item in native]
    binding_keys = [(item['target'], item['profile']) for item in expected_native_bindings]
    need(set(keys) == cells and len(set(keys)) == len(keys) and set(binding_keys) == cells and len(set(binding_keys)) == len(binding_keys), 'Missing or duplicate native matrix cell')
    need({item['target'] for item in reference} == set(TARGETS), 'Missing Java architecture')
    refs = {item['target']: item for item in reference}
    trusted = {(item['target'], item['profile']): item for item in expected_native_bindings}
    joins = [validate_join(item, refs[item['target']], expected_source, trusted[(item['target'], item['profile'])], root) for item in native]
    return bounded({'schema_version': 1, 'matrix': 'consistent', 'source': expected_source, 'pairs': joins,
                    'scope': 'Complete supplied 2architecture/2profile differential receipts; trusted controller verification remains required, no task acceptance or runtime authority.'})
