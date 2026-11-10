"""HOST-only object broker and frozen component controls. No HTTP/Docker workload."""
import copy
import json
from pathlib import Path
import runpy
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
M = runpy.run_path(str(ROOT / 'tools/object_durability_campaign.py'))
G = M['run_object'].__globals__

class FixtureDeadline:
    def require(self, seconds): pass


def checkpoint_options():
    return {'checkpoint': lambda *args: {'fixture': 'persisted'}, 'deadline': FixtureDeadline(),
            'checkpoint_context': {'claim': 'claim', 'source': source()}}

COMPONENT = runpy.run_path(str(ROOT / 'crates/testkit/research/test_object_sample_verify.py'))
# Integrate every unchanged author HOST control into normal tools discovery.
ObjectOracle = COMPONENT['ObjectOracle']
FaultOracle = COMPONENT['FaultOracle']
TransportBounds = COMPONENT['TransportBounds']
FullSyntheticMatrix = COMPONENT['FullSyntheticMatrix']


def source(): return {'clean': True, 'head': 'a' * 40, 'digest': 'b' * 64}


def record():
    return {'namespace': 'hsearch-m02-10-0123456789ab', 'owner': 'owner', 'image': 'sha256:' + 'c' * 64,
            'network': 'network-id', 'containers': ['d' * 64], 'endpoint': 'https://rustfs:9000', 'config_sha256': 'e' * 64}


def config(root=ROOT):
    r = record(); value = M['worker_config'](r, ROOT, root / 'harness', root / 'object-binding.json', root / 'object-evidence')
    value['target'] = 'x86_64-unknown-linux-gnu'
    return value


def value(root=ROOT): return M['inputs'](source(), 'f' * 40, record(), config(root), 'pinned-rustfs')


def worker_info(c):
    r = record()
    host = {'Memory': 4 * 1024**3, 'MemorySwap': 4 * 1024**3, 'NanoCpus': 2000000000, 'PidsLimit': 256,
            'ReadonlyRootfs': True, 'Privileged': False, 'NetworkMode': c['network'], 'PortBindings': {}, 'Devices': [],
            'CapAdd': [], 'CapDrop': ['ALL'], 'SecurityOpt': ['no-new-privileges'], 'PidMode': '', 'IpcMode': 'private',
            'Tmpfs': {'/tmp': 'rw,nosuid,nodev,size=67108864'}}
    mounts = [{'Destination': m['target'], 'Type': m['type'], 'Name' if m['type'] == 'volume' else 'Source': m['source'], 'RW': m['rw']} for m in c['mounts']]
    return {'Id': '1' * 64, 'Image': r['image'], 'HostConfig': host, 'Mounts': mounts,
            'Config': {'User': '10001:10001', 'WorkingDir': '/workspace', 'Labels': {'io.hsearch.owner': r['owner'], 'io.hsearch.namespace': r['namespace']},
                       'Cmd': ['python3', '/harness/execute.py', 'durability-object', '--disk-bytes', str(18 * 1024**3), '--timeout', '1200', '--output-bytes', '524288']},
            'NetworkSettings': {'Networks': {c['network']: {'NetworkID': r['network']}}},
            'State': {'Status': 'exited', 'Running': False, 'ExitCode': 0}}


class ObjectBrokerControls(unittest.TestCase):
    def test_final_binding_uses_actual_source_code_plan_image_topology_and_config(self):
        inputs = value(); binding = inputs['binding']; driver, oracle = M['modules']()
        M['validate_inputs'](inputs, record(), config(), 'pinned-rustfs')
        self.assertEqual(binding['source_head'], source()['head']); self.assertEqual(binding['source_sha256'], source()['digest'])
        self.assertEqual(binding['driver_sha256'], M['sha']((ROOT / M['DRIVER']).read_bytes()))
        self.assertEqual(binding['plan_sha256'], M['sha'](driver['canonical'](driver['plan']())))
        self.assertEqual(len(driver['cases']()), 144); self.assertEqual(len(oracle['matrix']()), 144)
        self.assertEqual(inputs['bucket'], 'hsearch-' + binding['run_id'])
        for key in ('config_sha256', 'topology_sha256', 'image_sha256', 'driver_sha256', 'oracle_sha256', 'plan_sha256', 'envelope_sha256'):
            wrong = copy.deepcopy(inputs); wrong['binding'][key] = '0' * 64
            with self.subTest(key=key), self.assertRaises(ValueError): M['validate_inputs'](wrong, record(), config(), 'pinned-rustfs')
        wrong = copy.deepcopy(inputs); wrong['binding']['execution_kind'] = 'synthetic'
        with self.assertRaises(ValueError): M['validate_inputs'](wrong, record(), config(), 'pinned-rustfs')

    def test_distinct18GiB_preserves_build_probe_and_current49GiB_refuses(self):
        sandbox = runpy.run_path(str(ROOT / 'tools/sandbox.py')); gib = sandbox['GIB']
        self.assertEqual(sandbox['BUILD']['disk_bytes'], 8 * gib)
        self.assertEqual(sandbox['PROBE'], {'cpu_millis': 500, 'memory_bytes': 256 * 1024**2, 'pids': 64, 'disk_bytes': 1024**2})
        self.assertEqual(sandbox['OBJECT'], M['BUDGET'])
        capacity = {'cpu_millis': 3000, 'memory_bytes': 8 * gib, 'pids': 2048, 'disk_bytes': 40 * gib}
        storage = {'budget': sandbox['STORAGE']}
        sandbox['admit'](sandbox['OBJECT'], [storage], capacity, 100 * gib)
        frozen = {'budget': {'cpu_millis': 0, 'memory_bytes': 0, 'pids': 0, 'disk_bytes': 21 * gib}}
        with self.assertRaisesRegex(sandbox['Refused'], 'disk_bytes budget exhausted'):
            sandbox['admit'](sandbox['OBJECT'], [storage, frozen], capacity, 100 * gib)

    def test_worker_exact_command_private_network_and_only_declared_mounts(self):
        c = config(); info = worker_info(c)
        M['verify_container'](info, record(), c)
        created = copy.deepcopy(info); created['State']['Status'] = 'created'
        created['NetworkSettings']['Networks'][c['network']]['NetworkID'] = ''
        M['verify_container'](created, record(), c)
        with self.assertRaises(ValueError): M['verify_container'](created, record(), c, require_endpoint=True)
        M['verify_container'](info, record(), c, require_endpoint=True)
        for mutate in (lambda i: i['HostConfig'].update(NetworkMode='bridge'), lambda i: i['HostConfig'].update(MemorySwap=-1),
                       lambda i: i['Config']['Cmd'].__setitem__(-5, str(8 * 1024**3)),
                       lambda i: i['Mounts'][0].update(RW=True), lambda i: i['Mounts'].append({'Destination': '/server-key'}),
                       lambda i: i['Config'].update(Entrypoint=['sh', '-c']), lambda i: i['HostConfig'].update(Privileged=True)):
            bad = copy.deepcopy(info); mutate(bad)
            with self.assertRaises((ValueError, KeyError)): M['verify_container'](bad, record(), c)
        mounted = {m['target'] for m in c['mounts']}
        self.assertEqual(mounted, {'/workspace', '/harness', '/object-input/binding.json', '/object-output', '/certs', '/run/secrets', '/object-store-accounting'})
        self.assertNotIn('/cargo', mounted); self.assertNotIn('/target', mounted)

    def test_monitor_inventory_refuses_private_or_unused_keys_without_exposing_credentials(self):
        execute = runpy.run_path(str(ROOT / 'dev/execute.py'))
        cert = b'-----BEGIN CERTIFICATE-----\nYWJjZA==\n-----END CERTIFICATE-----\n'
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); public = root / 'public'; private = root / 'private'; public.mkdir(); private.mkdir()
            for name in ('ca.crt', 'wrong-ca.crt'): (public / name).write_bytes(cert)
            (private / 'access').write_bytes(b'hs' + b'0' * 24); (private / 'secret').write_bytes(b'0' * 64)
            self.assertTrue(execute['object_mount_inventory'](public, private))
            (public / 'unused-client.key').write_bytes(b'dummy')
            with self.assertRaises(RuntimeError): execute['object_mount_inventory'](public, private)
            (public / 'unused-client.key').unlink(); (private / 'access').write_bytes(b'production-shaped-fixture')
            with self.assertRaisesRegex(RuntimeError, 'isolated development'): execute['object_mount_inventory'](public, private)

    def test_object_monitor_rejects_lowered_envelope_before_any_child(self):
        execute = runpy.run_path(str(ROOT / 'dev/execute.py'))
        for disk, seconds, output in ((8 * 1024**3, 1200, 524288), (18 * 1024**3, 1201, 524288), (18 * 1024**3, 1200, 1048577)):
            with patch.object(execute['subprocess'], 'Popen') as spawn, self.assertRaisesRegex(RuntimeError, 'distinct18GiB'):
                execute['run']('durability-object', disk, seconds, output)
            spawn.assert_not_called()
        self.assertEqual(execute['COMMANDS']['durability-object'], ['python3', '/workspace/' + M['DRIVER']])

    def test_object_dataset_stat_bound_refuses_before_spawn_and_public_copy_excludes_keys(self):
        execute = runpy.run_path(str(ROOT / 'dev/execute.py')); runtime = execute['run'].__globals__
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); (root / 'input.json').write_bytes(b'{}')
            def paths(value): return root / 'input.json' if str(value) == '/object-input/binding.json' else Path(value)
            with patch.object(execute['platform'], 'machine', return_value='x86_64'), patch.dict(runtime, verify_limits=lambda *a: {}, object_mount_inventory=lambda: True, Path=paths,
                            usage=lambda *a: 10 * 1024**3 + 1), \
                 patch.object(execute['os'], 'getuid', return_value=10001, create=True), \
                 patch.object(execute['os'], 'getgid', return_value=10001, create=True), \
                 patch.object(execute['subprocess'], 'Popen') as spawn, self.assertRaisesRegex(RuntimeError, 'separate10GiB'):
                execute['run']('durability-object', 18 * 1024**3, 1200, 524288)
            spawn.assert_not_called()
            raw = root / 'raw'; raw.mkdir(); (raw / 'index.json').write_bytes(b'{}'); (raw / 'server.key').write_bytes(b'dummy')
            with self.assertRaises(ValueError): M['publish'](raw, root / 'public', {'status': 'failed'}, b'bounded fixture log')
            self.assertFalse((root / 'public').exists())

    def test_topology_requires_real_owned_internal_network_and_healthy_pinned_store(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); raw = b'{}'; (root / 'compose.json').write_bytes(raw)
            r = record(); r['config_sha256'] = M['sha'](raw)
            network = {'Id': r['network'], 'Name': r['namespace'] + '-net', 'Internal': True,
                       'Labels': {'io.hsearch.owner': r['owner'], 'io.hsearch.namespace': r['namespace']}}
            store = {'Id': r['containers'][0], 'Image': 'storage-image', 'Config': {'Image': 'pinned-rustfs',
                     'Labels': {'com.docker.compose.service': 'rustfs', **network['Labels']}},
                     'State': {'Running': True, 'Health': {'Status': 'healthy'}}, 'HostConfig': {'PortBindings': {}},
                     'NetworkSettings': {'Ports': {}, 'Networks': {network['Name']: {'NetworkID': r['network']}}}}
            docker = SimpleNamespace(inspect=lambda kind, names: [network] if kind == 'network' else [{'Id': 'storage-image'}] if kind == 'image' else [store])
            broker = SimpleNamespace(docker=docker, snapshot=lambda r: root, images={'rustfs': 'pinned-rustfs'})
            self.assertEqual(M['verify_topology'](broker, r)['storage_cid'], r['containers'][0])
            network['Internal'] = False
            with self.assertRaises(ValueError): M['verify_topology'](broker, r)
            network['Internal'] = True; store['HostConfig']['PortBindings'] = {'9000/tcp': [{'HostPort': '9000'}]}
            with self.assertRaises(ValueError): M['verify_topology'](broker, r)

    def test_host_requires_actual_monitor_and_index_then_calls_independent_oracle(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); inputs = value(root); c = config(root)
            index = {'binding': inputs['binding'], 'files': []}; raw_index = M['encoded'](index); (root / 'index.json').write_bytes(raw_index)
            summary = {'object_cases_executed': 144, 'healthy_cases': 144, 'status': 'complete', 'index_sha256': M['sha'](raw_index)}
            receipt = {'mode': 'durability-object', 'exit': 0, 'peak_disk_bytes': 100, 'peak_dataset_bytes': 200,
                       'output_bytes': 100, 'elapsed_seconds': 1.5, 'object_input_sha256': M['sha'](M['encoded'](inputs)),
                       'object_mount_inventory': True, 'object_runtime': {'uid': 10001}}
            calls = []
            def oracle(directory, binding, execution):
                calls.append(execution)
                self.assertEqual(execution['worker_cid'], '1' * 64)
                self.assertEqual(execution['monitor']['scratch_bytes'], 18 * 1024**3)
                self.assertEqual(execution['runtime'], receipt['object_runtime'])
                return {'object_observations': 'verified', 'execution_kind': 'admitted-object', 'actual_case_records': 144, 'case_ids':['case-%03d'%i for i in range(144)]}
            with patch.dict(G, modules=lambda: ({}, {'verify': oracle})):
                raw = M['encoded'](summary) + M['encoded'](receipt)
                self.assertEqual(M['verify_export'](root, inputs, raw, '1' * 64, c)['object_campaign'], 'verified')
                self.assertEqual(len(calls), 1)
                for changed in (dict(receipt, peak_dataset_bytes=10 * 1024**3 + 1), dict(receipt, exit=1),
                                dict(receipt, object_input_sha256='0' * 64), dict(receipt, object_mount_inventory=False)):
                    with self.assertRaises(ValueError): M['verify_export'](root, inputs, M['encoded'](summary) + M['encoded'](changed), '1' * 64, c)
                with self.assertRaises(ValueError): M['verify_export'](root, inputs, raw + M['encoded'](receipt), '1' * 64, c)
                with self.assertRaises(ValueError): M['verify_export'](root, inputs, raw, 'unknown-cid', c)
            self.assertEqual(len(calls), 1)

    def test_partial_failure_inventory_and_normal_recovery_never_become144(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); output = root / 'object-evidence'; output.mkdir()
            raw_input = M['encoded'](value(root)); (root / 'object-binding.json').write_bytes(raw_input)
            (output / 'case-000.jsonl').write_bytes(b'{"partial":true}\n')
            pin = {'bytes': len(raw_input), 'sha256': M['sha'](raw_input)}
            retained = M['retain_failure'](root, b'bounded failed worker\n', '1' * 64, pin)
            self.assertEqual(retained, M['sha']((root / 'object-failure-retention.json').read_bytes()))
            failure = json.loads((root / 'object-failure-retention.json').read_text()); self.assertNotIn('object_cases', failure)
            self.assertEqual(len(failure['files']), 1)
            empty = root / 'empty'; empty.mkdir(); (empty / 'object-evidence').mkdir()
            (empty / 'object-binding.json').write_bytes(raw_input)
            M['retain_failure'](empty, b'', '2' * 64, pin)
            self.assertEqual((empty / 'object-worker.log').read_bytes(), b'')
            self.assertEqual(json.loads((empty / 'object-failure-retention.json').read_text())['raw_worker_sha256'], M['sha'](b''))
            (output / 'server.key').write_bytes(b'dummy')
            with self.assertRaises(ValueError): M['retain_failure'](root, b'bounded failed worker\n', '1' * 64, pin)

    def test_oracle_refusal_retains_raw_export_without_mutation(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); inputs = value(root); c = config(root)
            index = M['encoded']({'binding': inputs['binding'], 'files': []}); (root / 'index.json').write_bytes(index)
            summary = {'object_cases_executed': 144, 'healthy_cases': 144, 'status': 'complete', 'index_sha256': M['sha'](index)}
            receipt = {'mode': 'durability-object', 'exit': 0, 'peak_disk_bytes': 0, 'peak_dataset_bytes': 0, 'output_bytes': 0,
                       'elapsed_seconds': 0, 'object_input_sha256': M['sha'](M['encoded'](inputs)), 'object_mount_inventory': True, 'object_runtime': {}}
            def refuse(*args): raise ValueError('Raw counter mismatch')
            with patch.dict(G, modules=lambda: ({}, {'verify': refuse})), self.assertRaisesRegex(ValueError, 'Raw counter mismatch'):
                M['verify_export'](root, inputs, M['encoded'](summary) + M['encoded'](receipt), '1' * 64, c)
            self.assertEqual((root / 'index.json').read_bytes(), index)

    def test_object_scheduling_no_cargo_no_retry_and_failure_keeps_namespace(self):
        actual_run_path = runpy.run_path
        task = {'source_fingerprint': lambda *a: source(), 'git': lambda *a, **kw: source()['head'] if a[-1] == 'HEAD' else 'f' * 40}
        for fail in (False, True):
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve(); events = []; r = record()
                def up(*a, **kw): events.append('up'); return r['namespace']
                def run(namespace, mode, **kw):
                    self.assertEqual(mode, 'durability-object'); self.assertEqual(kw['output_budget'], 524288); events.append(mode)
                    if fail: raise ValueError('preserve failed worker')
                    return {'object_durability': {'oracle': {'faults': {'cas_attempts': 32}}}, 'container_id': '1' * 64, 'checkpoint': {'fixture': 'persisted'}}
                broker = SimpleNamespace(source_root=ROOT, up=up, run=run, down=lambda ns: events.append('down'), images={'rustfs': 'pinned-rustfs'},
                                         registry=SimpleNamespace(get=lambda ns: r, update=lambda *a, **kw: None), snapshot=lambda rec: root,
                                         harness=lambda rec: root / 'harness', docker=SimpleNamespace(info={'Architecture': 'x86_64'}))
                changes = []
                def route(path): return task if str(path).endswith('task.py') else actual_run_path(path)
                with patch.object(G['runpy'], 'run_path', side_effect=route):
                    if fail:
                        with self.assertRaisesRegex(ValueError, 'preserve failed worker'): M['run_object'](broker, 'claim', source(), changes.append, **checkpoint_options())
                    else:
                        result = M['run_object'](broker, 'claim', source(), changes.append, **checkpoint_options())
                        self.assertEqual(result['object_cases'], 144)
                self.assertEqual(events, ['up', 'durability-object'] if fail else ['up', 'durability-object', 'down'])
                self.assertEqual(changes[-1], r['namespace'] if fail else None)

    def test_real_broker_reserves18_and_oracle_precedes_disposal_and_recovery_retains_failure(self):
        sm = runpy.run_path(str(ROOT / 'tools/sandbox.py')); gib = sm['GIB']
        capacity = {'cpu_millis': 8000, 'memory_bytes': 16 * gib, 'pids': 2048, 'disk_bytes': 40 * gib}
        for fail in (False, True):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory() as temporary:
                events = []; r = record(); r.update(source=str(ROOT), engine='engine', phase='ready', snapshot_schema=2, resource_protocol=2)
                class Docker:
                    engine = 'engine'; info = {'Architecture': 'x86_64'}
                    def __init__(self): self.capacity = capacity; self.container = None
                    def command(self, *args, **kwargs):
                        events.append(args)
                        if args[0] == 'create':
                            self.container = worker_info(c); self.container['State'].update(Status='created')
                            self.args = args; return '1' * 64
                        if args[0] == 'start': self.container['State'].update(Status='running', Running=True)
                        if args[0] == 'wait':
                            self.container['State'].update(Status='exited', Running=False)
                            (folder / 'object-evidence/index.json').write_bytes(b'{}')
                            return '0'
                        if args[0] == 'logs':
                            self.log_bound = kwargs['output_bytes']; return {'stdout': b'public fixture worker log\n', 'stderr': b''}
                        if args[:2] == ('container', 'rm'): self.container = None
                        if args[:2] == ('container', 'ls'): return '1' * 64 if self.container else ''
                        return ''
                    def inspect(self, kind, names):
                        if kind == 'container': return [self.container]
                        return [{'Name': n, 'Labels': {'io.hsearch.owner': r['owner'], 'io.hsearch.namespace': r['namespace']}} for n in names]
                docker = Docker(); broker = sm['Sandbox'](Path(temporary), docker=docker, free_disk=lambda: 100 * gib)
                broker.registry.acquire(r['namespace'], sm['STORAGE'], capacity, 'engine', r)
                folder = broker.snapshot(r); folder.mkdir(parents=True); broker.harness = lambda rec: folder / 'harness'
                c = M['worker_config'](r, ROOT, broker.harness(r), folder / 'object-binding.json', folder / 'object-evidence')
                c['target'] = 'x86_64-unknown-linux-gnu'
                inputs = M['inputs'](source(), 'f' * 40, r, c, broker.images['rustfs'])
                raw = M['encoded'](inputs); (folder / 'object-binding.json').write_bytes(raw)
                broker.registry.update(r['namespace'], object_durability_input={'bytes': len(raw), 'sha256': M['sha'](raw)})
                broker.backend_free = lambda *a, **kw: 100 * gib
                def verify(*args):
                    events.append(('oracle',))
                    self.assertIsNotNone(docker.container)
                    with broker.registry.edit() as state:
                        leases = [v for v in state['leases'].values() if v.get('parent') == r['namespace']]
                    self.assertEqual(len(leases), 1); self.assertEqual(leases[0]['budget']['disk_bytes'], 18 * gib)
                    if fail: raise ValueError('retain bad object raw counters')
                    return {'oracle': {'faults': {'cas_attempts': 32}}, 'binding': inputs['binding']}
                module = dict(M, sync_file=lambda *a: events.append(('sync-file',)), sync_directory=lambda *a: events.append(('sync-directory',)), source_bookend=lambda *a: None, verify_topology=lambda *a: {'owned': True}, verify_export=verify)
                actual_run_path = runpy.run_path
                def route(path):
                    return module if str(path).replace('\\', '/').endswith(('tools/object_durability_campaign.py','tools/campaign_checkpoint.py')) else actual_run_path(path)
                with patch.object(sm['runpy'], 'run_path', side_effect=route):
                    if fail:
                        with self.assertRaisesRegex(ValueError, 'bad object raw counters'):
                            broker.run(r['namespace'], 'durability-object', artifact_directory=folder / 'object-evidence', stream=False, **checkpoint_options())
                        self.assertIsNotNone(docker.container)
                        with broker.registry.edit() as state:
                            children = [v for v in state['leases'].values() if v.get('parent') == r['namespace']]
                        self.assertFalse(children[0]['object_export_verified']); self.assertFalse(children[0]['owner_active'])
                        with self.assertRaisesRegex(sm['Refused'], 'checkpoint absent'):
                            broker.reconcile(r['namespace'])
                        self.assertIsNotNone(docker.container)
                        self.assertFalse(any(e[:2] == ('container', 'rm') for e in events))
                    else:
                        self.assertEqual(broker.run(r['namespace'], 'durability-object', artifact_directory=folder / 'object-evidence', stream=False, **checkpoint_options())['exit_code'], 0)
                        self.assertLess(events.index(('oracle',)), next(i for i, e in enumerate(events) if e[:2] == ('container', 'rm')))
                if not fail:self.assertIsNone(docker.container)
                self.assertEqual(docker.log_bound, 524288)
                args = docker.args
                self.assertEqual(args[args.index('--disk-bytes') + 1], str(18 * gib))
                self.assertEqual(args[args.index('--network') + 1], r['namespace'] + '-net')
                mounts = [args[i + 1] for i, a in enumerate(args) if a == '--mount']
                self.assertFalse(any('target=/cargo' in x or 'target=/target,' in x or '-server,' in x or '-ca,' in x for x in mounts))
