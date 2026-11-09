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
    execution_keys = {'execution_receipt_sha256'}
    need(set(native) == common | {'native_lucene_revision', 'image_id'} | execution_keys, 'Closed native receipt schema')
    need(set(reference) == common | {'pinned_image', 'image_id', 'server_version', 'controller_sha256'}, 'Closed reference receipt schema')
    for value, role in ((native, 'native'), (reference, 'reference')):
        need(type(value['schema_version']) is int and value['schema_version'] == (2 if role == 'native' else 1) and value['role'] == role and value['completed'] is True, 'Incomplete/wrong differential receipt')
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

# Native v2 is a prospective controller-bound receipt. Reference v1 is unchanged.
PROFILES = ('dev', 'release')
TOOL_KEYS = ('rustc', 'cargo', 'native_target', 'cargo_compiler', 'cargo_rustdoc',
             'nextest', 'nextest_executable', 'hashes', 'compiler_policy_sha256',
             'compiler_environment', 'compiler_wrappers', 'test_executor')


def capture_command(profile):
    need(profile in PROFILES, 'Unknown capture profile')
    return ['cargo', 'nextest', 'run', '--locked', '--offline', '--package', 'hsearch-engine',
            '--all-features', '--test', 'adapter_java_supplement',
            *(['--release'] if profile == 'release' else []), '--profile', 'hsearch',
            '--no-tests=fail', '--success-output', 'immediate']


def tool_context(tools, python):
    return bounded(dict({key: tools[key] for key in TOOL_KEYS}, python_executable=python))


def source_identity(root=ROOT):
    import runpy
    task = runpy.run_path(str(Path(root) / 'tools/task.py'))
    head = task['git']('rev-parse', 'HEAD', root=root).strip()
    state = task['source_fingerprint'](root, head, [])
    need(state['clean'], 'Clean differential source required')
    return {'head': head, 'tree': task['git']('rev-parse', 'HEAD^{tree}', root=root).strip(), 'source_digest': state['digest']}


def verify_tools(tools, image, root=ROOT):
    need(set(tools) == set(TOOL_KEYS) | {'python_executable'}, 'Closed qualified tool context required')
    target = tools['native_target']
    need(target in TARGETS and image['architecture'] == target.split('-')[0], 'Actual native architecture mismatch')
    need(tools['rustc'].startswith('rustc 1.97.1 ') and tools['cargo'].startswith('cargo 1.97.1 '), 'Exact native compiler required')
    policy = read_json(root, 'dev/correctness-tools.json')
    artifact = image['correctness_artifact']
    need(set(artifact) == {'architecture', 'revision', 'archive_sha256', 'profile_sha256', 'binary_sha256', 'version'}, 'Closed public correctness artifact')
    need(artifact['architecture'] == image['architecture'] and artifact['revision'] == policy['nextest']['revision'] and
         artifact['archive_sha256'] == policy['nextest']['architectures'][image['architecture']]['archive_sha256'] and
         artifact['profile_sha256'] == digest(policy) and artifact['version'] == tools['nextest'], 'Admitted nextest identity mismatch')
    need(tools['nextest'].split()[:2] == ['cargo-nextest', policy['nextest']['version']], 'Exact nextest required')
    expected_hashes = {'rustc', 'rustdoc', 'cargo', 'cargo_clippy', 'clippy_driver', 'cargo_fmt', 'rustfmt', 'llvm_cov', 'llvm_profdata', 'llvm_objdump', 'nextest'}
    need(set(tools['hashes']) == expected_hashes and all(SHA.fullmatch(x) and x != '0' * 64 for x in tools['hashes'].values()), 'Complete actual tool hashes required')
    need(tools['hashes']['nextest'] == artifact['binary_sha256'], 'Nextest binary mismatch')
    need(tools['compiler_policy_sha256'] == file_sha(root, 'harness/cargo-correctness.toml') and
         tools['compiler_wrappers'] == {'RUSTC_WRAPPER': '', 'RUSTC_WORKSPACE_WRAPPER': ''} and
         tools['compiler_environment'] == {'RUSTC_BOOTSTRAP': '-1', 'CLIPPY_ARGS': '-D__CLIPPY_HACKERY__warnings__CLIPPY_HACKERY__'}, 'Compiler policy mismatch')
    executor = tools['test_executor']
    need(set(executor) == {'path', 'sha256', 'environment_key', 'command'} and executor['path'] == '/usr/bin/env' and
         executor['command'] == '/usr/bin/env --' and executor['environment_key'] == 'CARGO_TARGET_' + target.upper().replace('-', '_') + '_RUNNER' and
         SHA.fullmatch(executor['sha256']) and executor['sha256'] != '0' * 64, 'Actual native test executor required')
    for key in ('cargo_compiler', 'cargo_rustdoc', 'nextest_executable', 'python_executable'):
        need(type(tools[key]) is str and tools[key].startswith('/') and not any(x.isspace() for x in tools[key]), 'Unqualified tool path')
    return target


def qualified_commands(tools, root=ROOT):
    """Reconstruct all fixed commands from the exact source, without executing them."""
    import runpy
    check = runpy.run_path(str(Path(root) / 'tools/check.py'))
    policy = '/workspace/harness/cargo-correctness.toml'
    def transform(command):
        command = [tools['python_executable'] if x == check['sys'].executable else x for x in command]
        if command[:2] == ['cargo', 'nextest']:
            return [tools['nextest_executable'], 'nextest', command[2], '--config', policy, *command[3:]]
        if command[:2] in (['cargo', 'clippy'], ['cargo', 'fmt']):
            name = 'cargo-clippy' if command[1] == 'clippy' else 'cargo-fmt'
            path = str(Path(tools['cargo_compiler']).parent / name)
            return [path, command[1], *(['--config', policy] if name == 'cargo-clippy' else []), *command[2:]]
        return [command[0], '--config', policy, *command[1:]] if command[0] == 'cargo' else command
    formatter = str(Path(tools['cargo_compiler']).parent / 'rustfmt')
    generator = [tools['python_executable'], 'crates/engine/tests/render_adapter_fixtures.py', '--root', '/workspace', '--check', '--formatter', formatter]
    research = runpy.run_path(str(Path(root) / 'tools/engine_research.py'))
    commands = [generator, *[transform(c) for c in check['CHECKS']],
                *[transform(c) for _, _, c in research['sample_commands'](Path(root))]]
    return [' '.join(c) for c in commands]


def command_proof(commands):
    # Full commands stay in the raw log. Reconstruct every command from exact source.
    captures = [c for c in commands if 'adapter_java_supplement' in c]
    need(len(captures) == 2, 'Both exact capture commands required')
    return {'sha256': digest(commands), 'count': len(commands), 'captures': captures}


def verify_execution(manifest, expected_source, root=ROOT):
    """Verify a supplied host manifest; caller trust is established by the controller."""
    bounded(manifest)
    need(set(manifest) == {'schema_version', 'source', 'image', 'container', 'tools', 'commands', 'monitor', 'result', 'raw_log', 'segments'}, 'Closed execution manifest required')
    need(set(expected_source) == {'head', 'tree', 'source_digest'} and all(type(v) is str and re.fullmatch(r'[0-9a-f]{64}' if k == 'source_digest' else r'[0-9a-f]{40}', v) for k, v in expected_source.items()), 'Exact clean source identity required')
    need(manifest['schema_version'] == 1 and type(manifest['schema_version']) is int and manifest['source'] == expected_source, 'Stale execution source')
    image, container = manifest['image'], manifest['container']
    need(set(image) == {'id', 'architecture', 'engine', 'build_inputs_sha256', 'dockerfile_sha256', 'correctness_artifact'}, 'Closed admitted image proof')
    need(type(image['id']) is str and re.fullmatch(r'sha256:[0-9a-f]{64}', image['id']) and type(image['engine']) is str and 0 < len(image['engine']) <= 128, 'Actual admitted image required')
    import runpy
    sandbox = runpy.run_path(str(Path(root) / 'tools/sandbox.py'))
    need(image['build_inputs_sha256'] == sandbox['image_digest']() and image['dockerfile_sha256'] == file_sha(root, 'dev/Dockerfile'), 'Stale admitted image inputs')
    need(set(container) == {'container_id', 'image_id', 'architecture', 'engine', 'limits', 'user', 'read_only', 'network', 'state', 'budget', 'output_budget'}, 'Closed public container proof')
    need(re.fullmatch(r'[0-9a-f]{64}', container['container_id']) and container['image_id'] == image['id'] and
         container['architecture'] == image['architecture'] and container['engine'] == image['engine'], 'Actual CID/image/architecture mismatch')
    need(container['user'] == '10001:10001' and container['read_only'] is True and container['network'] == 'none' and
         container['state'] == {'status': 'exited', 'running': False, 'exit_code': 0} and type(container['state']['exit_code']) is int and container['state']['running'] is False, 'Unqualified completed container')
    need(container['budget'] == sandbox['BUILD'] and type(container['output_budget']) is int and
         4096 <= container['output_budget'] <= 1024**2, 'Differential worker budget changed')
    limits = container['limits']
    need(set(limits) == {'cpu_nanos', 'memory', 'memory_swap', 'pids'} and
         all(type(x) is int for x in limits.values()) and 0 < limits['cpu_nanos'] <= 2_000_000_000 and
         0 < limits['memory'] <= 4 * 1024**3 and limits['memory_swap'] == limits['memory'] and 0 < limits['pids'] <= 256, 'Effective worker caps mismatch')
    target = verify_tools(manifest['tools'], image, root)
    need(manifest['commands'] == command_proof(qualified_commands(manifest['tools'], root)), 'Incomplete or foreign check commands')
    monitor, result, raw = manifest['monitor'], manifest['result'], manifest['raw_log']
    need(set(monitor) == {'mode', 'exit', 'elapsed_seconds', 'peak_disk_bytes', 'output_bytes'} and monitor['mode'] == 'check' and
         type(monitor['exit']) is int and monitor['exit'] == 0 and type(monitor['elapsed_seconds']) in {int, float} and
         0 <= monitor['elapsed_seconds'] <= 1200 and type(monitor['peak_disk_bytes']) is int and
         0 <= monitor['peak_disk_bytes'] <= 8 * 1024**3 and type(monitor['output_bytes']) is int and
         0 <= monitor['output_bytes'] <= container['output_budget'], 'Complete terminal monitor required')
    need(type(result.get('exit_code')) is int and result.get('truncated') is False and result == {'mode': 'check', 'exit_code': 0, 'truncated': False, 'container_id': container['container_id']}, 'Successful whole check required')
    need(set(raw) == {'sha256', 'bytes'} and SHA.fullmatch(raw['sha256']) and type(raw['bytes']) is int and
         monitor['output_bytes'] <= raw['bytes'] <= container['output_budget'], 'Bounded complete raw log required')
    need(set(manifest['segments']) == set(PROFILES) and all(SHA.fullmatch(x) for x in manifest['segments'].values()), 'Complete profile segment bindings required')
    return target


def collect_native(result, before, after, image, root=ROOT):
    """Called outside the worker with the real stock result and admitted image."""
    need(before == after, 'Source changed during native check')
    need(result['mode'] == 'check' and type(result['exit_code']) is int and result['exit_code'] == 0 and result['truncated'] is False, 'Successful untruncated outer check required')
    proof = result['execution']
    output = result['output']
    need(type(output) is str and len(output.encode()) <= proof['output_budget'] <= 1024**2, 'Native raw log bound')
    contexts, commands, monitors, cases = [], [], [], {p: [] for p in PROFILES}
    active, completed, captured_commands = None, [], {}
    for line in output.splitlines():
        need(len(line.encode()) <= CAP, 'Native log line bound')
        if line.startswith('CHECK: '):
            commands.append(line[7:])
            if active is not None:
                need(active not in captured_commands and 'adapter_java_supplement' in line and
                     ('--release' in line) == (active == 'release'), 'Capture command/profile mismatch')
                captured_commands[active] = line[7:]
            else:
                need('adapter_java_supplement' not in line, 'Unsegmented capture command')
        if any(key in line for key in ('engine_differential_native_case', 'engine_differential_tools', 'engine_differential_capture')):
            event = decode(line.encode())
            need(type(event) is dict and len(event) == 1, 'Closed native event envelope')
            if 'engine_differential_tools' in event:
                need(active is None and not contexts and not completed, 'Duplicate or late tool context')
                contexts.append(event['engine_differential_tools'])
            elif 'engine_differential_capture' in event:
                marker = event['engine_differential_capture']
                need(type(marker) is dict and set(marker) == {'profile', 'state'} and marker['profile'] in PROFILES, 'Capture delimiter shape')
                profile = marker['profile']
                if marker['state'] == 'started':
                    need(active is None and profile not in completed and profile == PROFILES[len(completed)], 'Cross-profile capture start')
                    active = profile
                else:
                    need(marker['state'] == 'completed' and active == profile and profile in captured_commands and len(cases[profile]) == 29, 'Capture completion without full successful command')
                    completed.append(profile)
                    active = None
            else:
                case = event['engine_differential_native_case']
                need(type(case) is dict and active in captured_commands and set(case) == {'id', 'outcome', 'fixture_sha256', 'value'} and
                     type(case['id']) is str and case['fixture_sha256'] == FIXTURE_SHA and case['outcome'] == 'pass', 'Foreign, failed or unsegmented native case')
                need(len(cases[active]) < 29 and case['id'] not in {x['id'] for x in cases[active]}, 'Duplicate or excess native case')
                cases[active].append({k: case[k] for k in ('id', 'outcome', 'value')})
        if line.startswith('{"mode":'):
            monitors.append(decode(line.encode()))
    need(active is None and completed == list(PROFILES) and len(contexts) == len(monitors) == 1, 'Incomplete native capture or monitor')
    lines = output.splitlines()
    need(len(lines) >= 2 and lines[-1].startswith('{"mode":') and
         lines[-2] == 'PASS: complete bootstrap suite; remaining milestone/runtime gates are recorded in the ledger', 'Complete ordinary suite required')
    bundle = load_fixture(root)
    for profile in PROFILES:
        need(len(cases[profile]) == 29 and {x['id'] for x in cases[profile]} == set(expected_cases(bundle)), 'Complete native case IDs required')
    manifest = bounded({'schema_version': 1, 'source': before, 'image': image, 'container': proof, 'tools': contexts[0],
        'commands': command_proof(commands), 'monitor': monitors[0], 'result': {k: result[k] for k in ('mode', 'exit_code', 'truncated', 'container_id')},
        'raw_log': {'bytes': len(output.encode()), 'sha256': hashlib.sha256(output.encode()).hexdigest()},
        'segments': {p: digest(cases[p]) for p in PROFILES}})
    target = verify_execution(manifest, before, root)
    natives = [bounded(dict(bindings(bundle), schema_version=2, role='native', source=before, target=target, profile=p,
        completed=True, cases=cases[p], native_lucene_revision=bundle['fixture']['pins']['native_lucene_revision'],
        image_id=image['id'], execution_receipt_sha256=digest(manifest))) for p in PROFILES]
    # Check actual typed values now, before publishing a host-collected receipt.
    for native in natives:
        for case in native['cases']:
            expected = expected_cases(bundle)[case['id']]
            need(canonical(normalize_metrics(case['value'], expected)) == canonical(expected), 'Native observed value mismatch')
    return bounded({'manifest': manifest, 'native': natives})


def architecture_package(capture, reference, root=ROOT):
    # Factor only identical source-derived bindings, never observed case values.
    shared = bindings(load_fixture(root))
    for native in capture['native']:
        need(all(native[key] == value for key, value in shared.items()), 'Native source bindings changed')
    return bounded({'schema_version': 2, 'bindings': shared, 'manifest': capture['manifest'],
                    'native': [{key: value for key, value in n.items() if key not in shared} for n in capture['native']],
                    'reference': reference})


def expanded_native(package, root=ROOT):
    shared = bindings(load_fixture(root))
    need(package['bindings'] == shared and all(not set(n).intersection(shared) for n in package['native']), 'Shared source transport bindings mismatch')
    return [dict(shared, **native) for native in package['native']]


def verify_package(package, expected_source, root=ROOT):
    bounded(package)
    need(set(package) == {'schema_version', 'bindings', 'manifest', 'native', 'reference'} and type(package['schema_version']) is int and package['schema_version'] == 2, 'Closed architecture package v2 required')
    target = verify_execution(package['manifest'], expected_source, root)
    need(type(package['native']) is list and len(package['native']) == 2 and
         [n['profile'] for n in package['native']] == list(PROFILES), 'Both native profiles required')
    trusted = []
    for native in expanded_native(package, root):
        profile = native['profile']
        binding = {'source': expected_source, 'target': target, 'profile': profile,
                   'image_id': package['manifest']['image']['id'], 'execution_receipt_sha256': digest(package['manifest'])}
        need(package['manifest']['segments'][profile] == digest(native['cases']), 'Observed segment mismatch')
        validate_join(native, package['reference'], expected_source, binding, root)
        trusted.append(binding)
    return trusted


def package_matrix(packages, expected_source, root=ROOT):
    bounded(packages)
    need(type(packages) is list and len(packages) == 2, 'Two actual architecture packages required')
    trusted = [binding for package in packages for binding in verify_package(package, expected_source, root)]
    return validate_matrix([n for p in packages for n in expanded_native(p, root)], [p['reference'] for p in packages], expected_source, trusted, root)


def evidence_directory(path, root=ROOT, private=None):
    path = Path(path).absolute()
    need(path.resolve() == path and not path.is_relative_to(Path(root).resolve()) and
         (private is None or not path.is_relative_to(Path(private).resolve())) and not path.exists(), 'Fresh external public evidence directory required')
    path.mkdir(parents=True, mode=0o700)
    return path


def save_public(path, value, limit=CAP):
    raw = canonical(value)
    need(len(raw) + 1 <= limit, 'Public artifact bound')
    with Path(path).open('xb') as stream:
        stream.write(raw + b'\n')


def package_cli():
    import argparse
    import os
    parser = argparse.ArgumentParser(description='Package existing host-collected public native and reference receipts; no workloads.')
    parser.add_argument('--native', type=Path, required=True)
    parser.add_argument('--reference-parent', type=Path, required=True)
    args = parser.parse_args()
    source = source_identity()
    capture = read_json(args.native, 'capture.json')
    from itertools import islice
    folders = list(islice(args.reference_parent.iterdir(), 2))
    need(len(folders) == 1 and folders[0].is_dir() and not folders[0].is_symlink(), 'One owned reference evidence directory required')
    # The existing reference report is larger than its differential projection.
    path = folders[0] / 'observations.json'
    need(path.resolve() == path.absolute() and path.is_file() and path.stat().st_size <= 1024**2, 'Bounded public reference observations required')
    report = json.loads(path.read_text(encoding='utf-8'), object_pairs_hook=strict_pairs)
    package = architecture_package(capture, report['engine_differential'])
    verify_package(package, source)
    need(source_identity() == source, 'Source changed while packaging')
    save_public(args.native / 'package.json', package)
    with Path(os.environ['GITHUB_OUTPUT']).open('a', encoding='utf-8') as stream:
        stream.write('differential=' + canonical(package).decode() + '\n')


def strict_pairs(rows):
    result = {}
    for key, value in rows:
        need(key not in result, 'Duplicate public JSON field')
        result[key] = value
    return result


if __name__ == '__main__':
    package_cli()
