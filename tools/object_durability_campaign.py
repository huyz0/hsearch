#!/usr/bin/env python3
"""Fixed object18GiB extension of the existing admitted M02.10 campaign broker."""
import hashlib
import json
from pathlib import Path
import re
import runpy
import secrets

ROOT = Path(__file__).resolve().parents[1]
N = runpy.run_path(str(ROOT / 'tools/native_durability_campaign.py'))
need, read, decode, encoded = (N[k] for k in ('need', 'read', 'decode', 'encoded'))
BUDGET = {'cpu_millis': 2000, 'memory_bytes': 4 * 1024**3, 'pids': 256, 'disk_bytes': 18 * 1024**3}
CAP, TOTAL = 1024**2, 32 * 1024**2
DRIVER = 'crates/testkit/research/object_workload.py'
ORACLE = 'crates/testkit/research/object_sample_verify.py'


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def modules(root=ROOT):
    return runpy.run_path(str(root / DRIVER)), runpy.run_path(str(root / ORACLE))


def worker_config(record, source, harness, input_path, output_path):
    """Closed, public configuration hashed before input creation; no secrets."""
    namespace = record['namespace']
    return {'mode': 'durability-object', 'image': record['image'], 'budget': BUDGET,
            'seconds': 1200, 'output_bytes': 512 * 1024, 'network': namespace + '-net',
            'argv': ['python3', '/workspace/' + DRIVER],
            'mounts': [
                {'target': '/workspace', 'type': 'bind', 'source': str(source).replace('\\', '/'), 'rw': False},
                {'target': '/harness', 'type': 'bind', 'source': str(harness).replace('\\', '/'), 'rw': False},
                {'target': '/object-input/binding.json', 'type': 'bind', 'source': str(input_path).replace('\\', '/'), 'rw': False},
                {'target': '/object-output', 'type': 'bind', 'source': str(output_path).replace('\\', '/'), 'rw': True},
                {'target': '/certs', 'type': 'volume', 'source': namespace + '-client', 'rw': False},
                {'target': '/run/secrets', 'type': 'volume', 'source': namespace + '-credentials', 'rw': False},
                # Public synthetic dataset: the monitor stats names/sizes only.
                {'target': '/object-store-accounting', 'type': 'volume', 'source': namespace + '-data', 'rw': False}]}


def topology(record, storage_image):
    return {key: record[key] for key in ('namespace', 'owner', 'network', 'containers', 'endpoint', 'config_sha256')} | {'storage_image': storage_image}


def inputs(source, tree, record, config, storage_image, root=ROOT):
    driver, oracle = modules(root)
    need(source['clean'] is True, 'Object campaign requires clean committed source')
    need(record['endpoint'] == 'https://rustfs:9000', 'Fixed private RustFS endpoint')
    run_id = secrets.token_hex(16)
    binding = {'run_id': run_id, 'source_head': source['head'], 'source_tree': tree, 'source_sha256': source['digest'],
               'lock_sha256': sha(read(root / 'Cargo.lock')), 'fixture_sha256': sha(read(root / driver['MODEL']['FIXTURE'])),
               'envelope_sha256': sha(N['ENVELOPE_TEXT'].encode()), 'image_sha256': record['image'].removeprefix('sha256:'),
               'topology_sha256': sha(encoded(topology(record, storage_image))), 'config_sha256': sha(encoded(config)),
               'driver_sha256': sha(read(root / DRIVER)), 'oracle_sha256': sha(read(root / ORACLE)),
               'plan_sha256': sha(driver['canonical'](driver['plan']())),
               'target': config['target'], 'execution_kind': 'admitted-object'}
    driver['binding_check'](binding); oracle['binding_check'](binding)
    need(len(driver['cases']()) == len(oracle['matrix']()) == 144, 'Original object144 matrix differs')
    need(N['ENVELOPE']['object_worker'] == {'cpu_millis': 2000, 'memory_bytes': 4294967296, 'pids': 256,
         'scratch_bytes': BUDGET['disk_bytes'], 'deadline_seconds': 1200, 'diagnostic_bytes': CAP}, 'Original object envelope differs')
    return {'schema': 1, 'binding': binding, 'bucket': 'hsearch-' + run_id}


def validate_inputs(value, record, config, storage_image, root=ROOT):
    driver, oracle = modules(root)
    need(type(value) is dict and set(value) == {'schema', 'binding', 'bucket'} and type(value['schema']) is int and value['schema'] == 1,
         'Closed object input schema')
    binding = value['binding']; driver['binding_check'](binding); oracle['binding_check'](binding)
    need(binding['execution_kind'] == 'admitted-object' and value['bucket'] == 'hsearch-' + binding['run_id'], 'Admitted owned bucket required')
    need(binding['image_sha256'] == record['image'].removeprefix('sha256:') and binding['target'] == config['target'] and
         binding['topology_sha256'] == sha(encoded(topology(record, storage_image))) and binding['config_sha256'] == sha(encoded(config)),
         'Object input actual image/target/topology/config differs')
    need(binding['envelope_sha256'] == sha(N['ENVELOPE_TEXT'].encode()) and binding['lock_sha256'] == sha(read(root / 'Cargo.lock')) and
         binding['fixture_sha256'] == sha(read(root / driver['MODEL']['FIXTURE'])) and binding['driver_sha256'] == sha(read(root / DRIVER)) and
         binding['oracle_sha256'] == sha(read(root / ORACLE)) and binding['plan_sha256'] == sha(driver['canonical'](driver['plan']())),
         'Object input source/plan/fixture/envelope differs')


def source_bookend(root, binding):
    task = runpy.run_path(str(root / 'tools/task.py'))
    head = task['git']('rev-parse', 'HEAD', root=root).strip()
    tree = task['git']('rev-parse', 'HEAD^{tree}', root=root).strip()
    source = task['source_fingerprint'](root, head, [])
    need(source['clean'] and source['digest'] == binding['source_sha256'] and head == binding['source_head'] and tree == binding['source_tree'],
         'Object campaign source changed')


def verify_topology(sandbox, record):
    need(sha(read(sandbox.snapshot(record) / 'compose.json')) == record['config_sha256'], 'Owned topology snapshot changed')
    network = sandbox.docker.inspect('network', [record['network']])
    need(len(network) == 1 and network[0]['Id'] == record['network'] and network[0]['Name'] == record['namespace'] + '-net' and
         network[0]['Internal'] is True and network[0]['Labels'].get('io.hsearch.owner') == record['owner'] and
         network[0]['Labels'].get('io.hsearch.namespace') == record['namespace'], 'Object network is not the owned private network')
    containers = sandbox.docker.inspect('container', record['containers'])
    stores = [item for item in containers if item['Config']['Labels'].get('com.docker.compose.service') == 'rustfs']
    need(len(stores) == 1, 'Exactly one acknowledged private RustFS required')
    store = stores[0]
    image = sandbox.docker.inspect('image', [sandbox.images['rustfs']])
    need(len(image) == 1 and store['Image'] == image[0]['Id'] and store['Config']['Image'] == sandbox.images['rustfs'] and
         store['Id'] in record['containers'] and store['Config']['Labels'].get('io.hsearch.owner') == record['owner'] and
         store['Config']['Labels'].get('io.hsearch.namespace') == record['namespace'] and store['State']['Running'] is True and
         store['State']['Health']['Status'] == 'healthy' and not store['HostConfig']['PortBindings'] and
         not any(store['NetworkSettings']['Ports'].values()) and
         set(store['NetworkSettings']['Networks']) == {record['namespace'] + '-net'} and
         store['NetworkSettings']['Networks'][record['namespace'] + '-net']['NetworkID'] == record['network'],
         'Private pinned RustFS image/health/owner/network differs')
    return {'network_id': record['network'], 'storage_cid': store['Id'], 'storage_image': store['Image']}


def verify_container(info, record, config, *, require_endpoint=False):
    host, actual = info['HostConfig'], info['Config']
    need(info['Image'] == record['image'] and actual['User'] == '10001:10001' and host['Memory'] == BUDGET['memory_bytes'] and
         host['MemorySwap'] == BUDGET['memory_bytes'] and host['NanoCpus'] == 2000000000 and host['PidsLimit'] == 256 and
         host['ReadonlyRootfs'] is True and host['Privileged'] is False and host['NetworkMode'] == config['network'] and
         not host.get('PortBindings') and not host.get('Devices') and not host.get('CapAdd') and set(host['CapDrop']) == {'ALL'} and
         host['SecurityOpt'] == ['no-new-privileges'] and host.get('PidMode', '') == '' and host.get('IpcMode') == 'private' and
         not actual.get('ExposedPorts') and not actual.get('Volumes') and host.get('Tmpfs') == {'/tmp': 'rw,nosuid,nodev,size=67108864'},
         'Object worker image/user/resource/isolation differs')
    need(not actual.get('Entrypoint') and actual['WorkingDir'] == '/workspace' and actual['Cmd'] ==
         ['python3', '/harness/execute.py', 'durability-object', '--disk-bytes', str(BUDGET['disk_bytes']), '--timeout', '1200',
          '--output-bytes', str(config['output_bytes'])], 'Object worker fixed monitor command differs')
    need(set(info['NetworkSettings']['Networks']) == {config['network']}, 'Object worker network differs')
    endpoint = info['NetworkSettings']['Networks'][config['network']]['NetworkID']
    # A created/stopped container can lack a live endpoint. The broker also
    # inspects after start and requires the exact independently owned network ID.
    need(endpoint == record['network'] or not require_endpoint and endpoint == '' and
         info['State']['Status'] in {'created', 'exited', 'dead'}, 'Object worker network endpoint differs')
    expected = {m['target']: m for m in config['mounts']}; seen = set()
    for mount in info['Mounts']:
        target = mount['Destination']
        if target == '/tmp' and mount['Type'] == 'tmpfs':
            continue
        need(target in expected and target not in seen, 'Unlisted/duplicate object worker mount')
        entry = expected[target]
        origin = mount.get('Name') if mount['Type'] == 'volume' else mount['Source'].replace('\\', '/')
        need(mount['Type'] == entry['type'] and mount['RW'] is entry['rw'] and origin == entry['source'], 'Object worker mount differs')
        seen.add(target)
    need(seen == set(expected), 'Missing object worker mount')


def inventory(directory, *, require_index=True):
    need(directory.is_dir() and directory.resolve() == directory and not directory.is_symlink(), 'Object export directory')
    result = {}; total = 0
    for path in directory.iterdir():
        need(len(result) < 256 and re.fullmatch(r'index\.json|run\.jsonl|faults\.jsonl|case-(?:0[0-9]{2}|1[0-3][0-9]|14[0-3])\.jsonl', path.name),
             'Object export inventory')
        raw = read(path); total += len(raw); need(total <= TOTAL, 'Object export total bound')
        result[path.name] = {'file': path.name, 'bytes': len(raw), 'sha256': sha(raw)}
    need(not require_index or 'index.json' in result, 'Object index missing')
    return result


def retain_failure(snapshot, raw_logs, cid, input_pin):
    """Normal known-terminal recovery retains bounded failures, never a pass."""
    need(re.fullmatch('[a-f0-9]{64}', cid) and type(raw_logs) is bytes and len(raw_logs) <= 512 * 1024,
         'Bounded object failure log/CID required')
    raw_input = read(snapshot / 'object-binding.json', 65536)
    need(input_pin == {'bytes': len(raw_input), 'sha256': sha(raw_input)}, 'Retained object input changed')
    output = snapshot / 'object-evidence'
    files = inventory(output, require_index=False)
    result = {'status': 'failed-or-interrupted; not qualification', 'worker_cid': cid,
              'input_sha256': sha(raw_input), 'raw_worker_sha256': sha(raw_logs), 'files': list(files.values())}
    raw_result = encoded(result)
    need(len(raw_result) <= CAP and sum(r['bytes'] for r in files.values()) + len(raw_logs) + len(raw_result) <= TOTAL,
         'Object failure retention quota')
    for name, raw in (('object-worker.log', raw_logs), ('object-failure-retention.json', raw_result)):
        path = snapshot / name
        if path.exists():
            need(read(path, allow_empty=name == 'object-worker.log') == raw, 'Existing object failure evidence changed')
        else:
            with path.open('xb') as stream: stream.write(raw)
    need(inventory(output, require_index=False) == files, 'Object failure extents changed during retention')
    return sha(raw_result)


def verify_export(directory, inputs_value, raw_logs, cid, config):
    """Called by the broker only after actual CID/caps/topology inspections."""
    need(re.fullmatch('[a-f0-9]{64}', cid) and type(raw_logs) is bytes and 0 < len(raw_logs) <= config['output_bytes'], 'Object CID/raw worker bound')
    frames = [decode(line) for line in raw_logs.splitlines() if line]
    summaries = [v for v in frames if type(v) is dict and 'object_cases_executed' in v]
    monitors = [v for v in frames if type(v) is dict and v.get('mode') == 'durability-object']
    need(len(frames) == 2 and len(summaries) == len(monitors) == 1, 'Exactly one child summary and independent monitor receipt required')
    summary, monitor = summaries[0], monitors[0]
    need(summary['object_cases_executed'] == summary['healthy_cases'] == 144 and summary['status'] == 'complete' and
         monitor['exit'] == 0 and type(monitor['peak_disk_bytes']) is int and 0 <= monitor['peak_disk_bytes'] <= BUDGET['disk_bytes'] and
         type(monitor['peak_dataset_bytes']) is int and 0 <= monitor['peak_dataset_bytes'] <= 10 * 1024**3 and
         type(monitor['output_bytes']) is int and 0 <= monitor['output_bytes'] <= config['output_bytes'] and
         type(monitor['elapsed_seconds']) in (int, float) and 0 <= monitor['elapsed_seconds'] <= 1200 and
         monitor['object_input_sha256'] == sha(encoded(inputs_value)) and monitor['object_mount_inventory'] is True,
         'Object completion/monitor proof differs')
    files = inventory(directory); index_raw = read(directory / 'index.json'); index = decode(index_raw)
    need(summary['index_sha256'] == sha(index_raw) and index['binding'] == inputs_value['binding'], 'Actual object output index differs')
    execution = {'kind': 'admitted-object', 'index_sha256': sha(index_raw), 'bucket': inputs_value['bucket'], 'worker_cid': cid,
                 'image_sha256': inputs_value['binding']['image_sha256'], 'raw_worker_sha256': sha(raw_logs), 'runtime': monitor['object_runtime'],
                 'monitor': {'cpu_millis': 2000, 'memory_bytes': 4 * 1024**3, 'added_swap_bytes': 0, 'pids': 256,
                             'scratch_bytes': BUDGET['disk_bytes'], 'seconds': 1200, 'output_bytes': config['output_bytes'], 'exit_code': 0, 'truncated': False},
                 'export_files': index['files']}
    _, oracle = modules()
    result = oracle['verify'](directory, inputs_value['binding'], execution)
    need(result['object_observations'] == 'verified' and result['execution_kind'] == 'admitted-object' and result['actual_case_records'] == 144 and result['case_ids']==['case-%03d'%i for i in range(144)],
         'Independent object matrix incomplete')
    need(inventory(directory) == files, 'Object raw export changed during independent verification')
    proof = {'object_campaign': 'verified', 'binding': inputs_value['binding'], 'execution': execution, 'oracle': result,
             'monitor_receipt': monitor, 'inventory': list(files.values()), 'scope': 'One native architecture only; no provider/product/fleet qualification.'}
    need(len(encoded(proof)) <= CAP and sum(row['bytes'] for row in files.values()) + len(encoded(proof)) + len(raw_logs) <= TOTAL,
         'Object raw/execution/diagnostic artifact quota differs')
    return proof


def publish(directory, destination, proof, raw_logs):
    files = inventory(directory)
    payloads = {name: read(directory / name) for name in files}
    payloads.update({'execution.json': encoded(proof), 'worker.log': raw_logs})
    need(len(payloads) <= 256 and all((0 if n == 'worker.log' else 1) <= len(v) <= CAP for n, v in payloads.items()) and sum(map(len, payloads.values())) <= TOTAL,
         'Whole public object export including execution/log exceeds original quota')
    need(not destination.exists() and destination.parent.resolve() == destination.parent, 'Fresh public object output required')
    destination.mkdir(mode=0o700)
    for name, raw in payloads.items():
        with (destination / name).open('xb') as stream: stream.write(raw)
    need(all(read(destination / name, allow_empty=name == 'worker.log') == raw for name, raw in payloads.items()), 'Object public transfer mismatch')


def run_object(sandbox, claim, source, namespace_changed=lambda value: None, public_output=None, *, checkpoint=None, deadline=None, checkpoint_context=None):
    need(callable(checkpoint) and deadline is not None and type(checkpoint_context) is dict, 'Object campaign requires trusted checkpoint')
    deadline.require(1250 + 12 * 180 + 600)
    task = runpy.run_path(str(ROOT / 'tools/task.py'))
    need(task['source_fingerprint'](sandbox.source_root, source['head'], []) == source, 'Stale object campaign source')
    namespace = sandbox.up('M02.10', task_claim=claim); namespace_changed(namespace)
    record = sandbox.registry.get(namespace); snapshot = sandbox.snapshot(record)
    input_path = snapshot / 'object-binding.json'; output_path = snapshot / 'object-evidence'
    config = worker_config(record, sandbox.source_root, sandbox.harness(record), input_path, output_path)
    config['target'] = sandbox.docker.info['Architecture'] + '-unknown-linux-gnu'
    tree = task['git']('rev-parse', 'HEAD^{tree}', root=sandbox.source_root).strip()
    value = inputs(source, tree, record, config, sandbox.images['rustfs'], sandbox.source_root)
    raw = encoded(value); need(len(raw) <= 65536, 'Object input byte bound')
    with input_path.open('xb') as stream: stream.write(raw)
    input_path.chmod(0o444)
    sandbox.registry.update(namespace, object_durability_input={'bytes': len(raw), 'sha256': sha(raw)})
    # No Cargo/prefetch/build gate: this is the fixed Python object driver only.
    result = sandbox.run(namespace, 'durability-object', output_budget=config['output_bytes'], stream=False, artifact_directory=output_path, checkpoint=checkpoint, deadline=deadline, checkpoint_context=checkpoint_context)
    proof = result['object_durability']; source_bookend(sandbox.source_root, value['binding'])
    deadline.require(20*180)
    sandbox.down(namespace); namespace_changed(None)
    return {'object_cases': 144, 'faults': proof['oracle']['faults'], 'container_id': result['container_id'],
            'checkpoint': result['checkpoint'], 'run_id':value['binding']['run_id'],
            'case_ids_pointer':'/object_durability/oracle/case_ids', 'target': value['binding']['target']}
