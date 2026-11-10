"""Synthetic HOST campaign controls; never invoke Cargo, Docker or native samples."""
import copy
import json
from pathlib import Path
import runpy
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
M = runpy.run_path(str(ROOT / 'tools/native_durability_campaign.py'))
G = M['run_campaign'].__globals__
WITNESS = runpy.run_path(str(ROOT / 'crates/testkit/research/test_transaction_witness.py'))
TransactionTests = WITNESS['TransactionTests']
BundleContentBarrierControls = WITNESS['BundleContentBarrierControls']

class FixtureDeadline:
    def require(self, seconds): pass


def checkpoint_options():
    return {'checkpoint': lambda *args: {'fixture': 'persisted'}, 'deadline': FixtureDeadline(),
            'checkpoint_context': {'claim': 'claim', 'source': source()}}



def source():
    return {'clean': True, 'head': 'a' * 40, 'digest': 'b' * 64}


def spec():
    return M['binding'](source(), 'sha256:' + 'c' * 64, M['matrix'](M['TARGETS'][0])[0])


class CampaignControls(unittest.TestCase):
    def setUp(self):
        # These existing tests isolate native scheduling. The object broker has
        # separate real-oracle HOST controls and never runs an HTTP workload here.
        self.object_patch = patch.dict(G, object_campaign=lambda sandbox, *a, **kw: {
            'object_cases': 144, 'target': sandbox.docker.info['Architecture'] + '-unknown-linux-gnu'})
        self.object_patch.start(); self.addCleanup(self.object_patch.stop)

    def test_bare_zero_exit_and_changed_ordinary_monitor_cannot_be_a_checkpoint_proof(self):
        budget={'cpu_millis':2000,'memory_bytes':4*1024**3,'pids':256,'disk_bytes':8*1024**3}
        execution={'container_id':'a'*64,'budget':budget,'user':'10001:10001','read_only':True,'network':'bridge',
                   'limits':{'cpu_nanos':2000000000,'memory':4*1024**3,'memory_swap':4*1024**3,'pids':256},'architecture':'x86_64'}
        monitor={'mode':'prefetch','exit':0,'output_bytes':20,'peak_disk_bytes':100,'elapsed_seconds':1}
        result={'container_id':'a'*64,'execution':execution,'raw_streams':{'stdout':M['encoded'](monitor),'stderr':b''}}
        self.assertEqual(M['verify_ordinary'](result,'prefetch',ROOT)['mode'],'prefetch')
        for failure in ('bare-zero','stderr','duplicate','wrong-limit','missing-compile'):
            bad=copy.deepcopy(result)
            if failure=='bare-zero':bad['raw_streams']['stdout']=b'completed\n'
            if failure=='stderr':bad['raw_streams']['stderr']=b'synthetic warning'
            if failure=='duplicate':bad['raw_streams']['stdout']*=2
            if failure=='wrong-limit':bad['execution']['limits']['memory']*=2
            if failure=='missing-compile':
                bad['execution']['network']='none';bad['raw_streams']['stdout']=M['encoded'](dict(monitor,mode='check'))
            with self.subTest(failure=failure),self.assertRaises(ValueError):
                M['verify_ordinary'](bad,'check' if failure=='missing-compile' else 'prefetch',ROOT)

    def test_twenty_seven_attempts_map_to_named_original_controls_not_counts_alone(self):
        contexts=['workload-%02d'%i for i in range(18)]+['failure']+[
            'scale-%02d-%02d'%(g,i) for g in (1,4,16) for i in range(g)]
        self.assertEqual(M['context_ids'](),contexts)
        self.assertEqual(len(set(contexts)),40)
        self.assertEqual(M['UNSAFE_IDS'],('false-flush-or-advance','send-before-persist','uncommitted-apply',
            'blind-new-identity','missing-response','forged-state-witness','insufficient-four-voter-quorum'))
        for target in M['TARGETS']:
            plan=M['attempt_plan'](target)
            self.assertEqual([r['event'] for r in plan],['attempt-%02d'%i for i in range(27)])
            self.assertEqual([r['mode'] for r in plan[:3]],['prefetch','check','durability-object'])
            cells=[r['cell'] for r in plan if r['mode']=='durability-sample']
            self.assertEqual(cells,M['matrix'](target))
            for i,cell in enumerate(cells):
                self.assertEqual(plan[3+i*2]['cell'],cell)
                self.assertEqual(plan[3+i*2]['mode'],'prefetch')
                self.assertEqual(plan[4+i*2]['cell'],cell)
            self.assertEqual((len(cells)*len(contexts),len(cells)*len(M['UNSAFE_IDS'])),(480,84))

    def test_exact_twelve_cells_per_architecture_and_fixed_single_candidate_argv(self):
        all_cells = []
        for target in M['TARGETS']:
            cells = M['matrix'](target)
            self.assertEqual(len(cells), 12)
            self.assertEqual(len({tuple(row.items()) for row in cells}), 12)
            for row in cells:
                argv = M['sample_command'](row)
                self.assertEqual(argv[:10], ['cargo', 'nextest', 'run', '--locked', '--offline', '--package', 'hsearch-testkit', '--all-features', '--test', 'native_durability'])
                self.assertEqual('--release' in argv, row['profile'] == 'release')
                self.assertEqual(argv[argv.index('-E') + 1], 'test(=' + M['CANDIDATES'][row['candidate']] + ')')
                self.assertNotIn('--no-run', argv)
            all_cells += cells
        self.assertEqual(len(all_cells), 24)
        with self.assertRaises(ValueError): M['matrix']('x86_64-pc-windows-msvc')

    def test_input_is_closed_and_cannot_claim_uuid_or_diagnostic_execution(self):
        good = spec()
        for key, value in [('nextest_run_id', 'd' * 36), ('execution', 'diagnostic-cargo-test'), ('repetition', True),
                           ('repetition', 4), ('target', 'wasm32'), ('profile', 'custom'), ('candidate', 'other'),
                           ('envelope_sha256', 'd' * 64), ('fixture_sha256', 'e' * 64), ('image', 'latest')]:
            with self.subTest(key=key, value=value), self.assertRaises((ValueError, KeyError)):
                M['validate_input'](dict(good, **{key: value}))
        with self.assertRaises(ValueError): M['validate_input'](dict(good, arbitrary_env='yes'))
        with self.assertRaises(ValueError): M['binding'](dict(source(), clean=False), good['image'], M['matrix'](good['target'])[0])

    def test_one_sample_is_parsed_across_both_bounded_streams_without_relabeling(self):
        row = {'sample_schema': 2, 'runtime': {'strict': True}}
        raw = b'stdout diagnostics\nNATIVE_DURABILITY_SAMPLE: ' + json.dumps(row).encode() + b'\n'
        self.assertEqual(M['parse_sample'](raw), row)
        for bad in (b'', raw + raw, b'x' * (512 * 1024 + 1)):
            with self.assertRaises(ValueError): M['parse_sample'](bad)
        for bad in (b'{"a":1,"a":2}', b'[' * 33 + b'0' + b']' * 33, b'{"a":NaN}'):
            with self.assertRaises(ValueError): M['parse_sample'](b'NATIVE_DURABILITY_SAMPLE: ' + bad)

    def test_full40_actual_content_oracle_rejects_valid_unrelated_sync_prefix_for_both_candidates(self):
        tape_module = runpy.run_path(str(ROOT / 'crates/testkit/research/test_transaction_witness.py'))
        bundle, verifier = M['modules']()
        labels = [('context-%02d' % i, {}) for i in range(40)]
        for candidate in M['CANDIDATES']:
            tape = tape_module['history'](candidate)
            verifier['F']['order_events'](tape.rows, candidate, tape.files)
            files = dict(tape.files, **{'artifacts/events-000.ndjson': b''.join(M['encoded'](r) for r in tape.rows)})
            original = copy.deepcopy(files)
            with patch.dict(G, modules=lambda: (dict(bundle, cases=lambda sample: labels), verifier)):
                report = M['full40_control']({'sample': {'candidate': candidate}}, {label: dict(files) for label, _ in labels})
                self.assertEqual(report['count'], 40)
                self.assertTrue(all(r['physical_prefix'] == 'valid' and r['result'] == 'refused' for r in report['contexts']))
            self.assertEqual(files, original)

    def test_full40_control_fails_if_content_ledger_accepts_or_refuses_for_another_reason(self):
        tape_module = runpy.run_path(str(ROOT / 'crates/testkit/research/test_transaction_witness.py'))
        bundle, verifier = M['modules'](); tape = tape_module['history']('raft-rs')
        files = dict(tape.files, **{'artifacts/events-000.ndjson': b''.join(M['encoded'](r) for r in tape.rows)})
        labels = [('context-%02d' % i, {}) for i in range(40)]
        original = verifier['F']['COMMIT']['verify']
        try:
            for action in (lambda *a: {}, lambda *a: (_ for _ in ()).throw(ValueError('unrelated parser failure'))):
                verifier['F']['COMMIT']['verify'] = action
                with patch.dict(G, modules=lambda: (dict(bundle, cases=lambda sample: labels), verifier)), self.assertRaises(ValueError):
                    M['full40_control']({'sample': {'candidate': 'raft-rs'}}, {label: dict(files) for label, _ in labels})
        finally:
            verifier['F']['COMMIT']['verify'] = original

    def fake(self, root, *, fail=None, duplicate=False):
        events = []; records = {}; counter = [0]
        def up(task, task_claim=None):
            counter[0] += 1; namespace = 'sample-' + str(counter[0]); events.append(('up', namespace))
            (root / namespace).mkdir(); records[namespace] = {'namespace': namespace, 'image': 'sha256:' + 'c' * 64}
            return namespace
        def run(namespace, mode, **kwargs):
            events.append((mode, namespace))
            if mode == 'durability-sample':
                if counter[0] == fail: raise ValueError('retained failed worker')
                binding = json.loads((root / namespace / 'durability-binding.json').read_text())
                binding.update(nextest_run_id='00000000-0000-0000-0000-%012d' % (1 if duplicate else counter[0]),
                               protoc_sha256=M['PROTOC'][binding['target']])
                return {'container_id': '%064x' % counter[0], 'native_durability': {'binding': binding}, 'checkpoint': {'fixture': 'persisted'}}
            return {}
        sandbox = SimpleNamespace(source_root=ROOT, docker=SimpleNamespace(info={'Architecture': 'x86_64'}), up=up, run=run,
                                  down=lambda ns: events.append(('down', ns)), snapshot=lambda record: root / record['namespace'],
                                  registry=SimpleNamespace(get=lambda ns: records[ns], update=lambda ns, **kw: records[ns].update(kw)))
        return sandbox, events

    def test_serial_fresh_workers_targets_uuid_bindings_and_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            sandbox, events = self.fake(Path(temporary).resolve()); changes = []
            with patch.object(G['runpy'], 'run_path', return_value={'source_fingerprint': lambda *a: source()}):
                result = M['run_campaign'](sandbox, 'claim', source(), changes.append, **checkpoint_options())
            self.assertEqual((result['sample_count'], result['temporal_contexts'], result['unsafe_controls']), (12, 480, 84))
            self.assertEqual((result['full40_replay_sets'], result['full40_context_replays'], result['full40_replays']), (12, 480, 480))
            self.assertEqual(len(changes), 24)
            for i in range(12):
                ns = 'sample-' + str(i + 1)
                self.assertEqual(events[i * 4:(i + 1) * 4], [('up', ns), ('prefetch', ns), ('durability-sample', ns), ('down', ns)])
                binding = json.loads((Path(temporary) / ns / 'durability-binding.json').read_text())
                self.assertIsNone(binding['nextest_run_id'])

    def test_failed_or_duplicate_sample_stops_without_retry_or_disposal(self):
        for options in ({'fail': 3}, {'duplicate': True}):
            with self.subTest(options=options), tempfile.TemporaryDirectory() as temporary:
                sandbox, events = self.fake(Path(temporary).resolve(), **options); changes = []
                with patch.object(G['runpy'], 'run_path', return_value={'source_fingerprint': lambda *a: source()}), self.assertRaises(ValueError):
                    M['run_campaign'](sandbox, 'claim', source(), changes.append, **checkpoint_options())
                self.assertEqual(events[-1][0], 'durability-sample')
                self.assertIsNotNone(changes[-1])
                self.assertEqual(sum(e[0] == 'up' for e in events), 3 if 'fail' in options else 2)

    def test_stale_source_refuses_before_allocation(self):
        with tempfile.TemporaryDirectory() as temporary:
            sandbox, events = self.fake(Path(temporary).resolve())
            with patch.object(G['runpy'], 'run_path', return_value={'source_fingerprint': lambda *a: dict(source(), digest='d' * 64)}), self.assertRaises(ValueError):
                M['run_campaign'](sandbox, 'claim', source(), **checkpoint_options())
            self.assertEqual(events, [])

    def test_object_matrix_must_finish_before_native_samples_and_failure_never_retries(self):
        for fail in (False, True):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory() as temporary:
                sandbox, events = self.fake(Path(temporary).resolve())
                def objects(*args, **kwargs):
                    events.append(('object144', 'closed' if not fail else 'retained-failure'))
                    if fail: raise ValueError('object failure')
                    return {'object_cases': 144, 'target': 'x86_64-unknown-linux-gnu'}
                with patch.dict(G, object_campaign=objects), \
                     patch.object(G['runpy'], 'run_path', return_value={'source_fingerprint': lambda *a: source()}):
                    if fail:
                        with self.assertRaisesRegex(ValueError, 'object failure'): M['run_campaign'](sandbox, 'claim', source(), **checkpoint_options())
                        self.assertEqual(events, [('object144', 'retained-failure')])
                    else:
                        result = M['run_campaign'](sandbox, 'claim', source(), **checkpoint_options())
                        self.assertEqual(events[0], ('object144', 'closed'))
                        self.assertEqual(events[1], ('up', 'sample-1'))
                        self.assertEqual(result['object144']['object_cases'], 144)

    def test_engine_caps_image_mounts_and_network_are_inspected_not_inferred(self):
        record = {'image': 'sha256:' + 'c' * 64, 'namespace': 'owned'}
        source_root, harness, input_path, output_path = map(Path, ['/source', '/harness', '/input.json', '/output'])
        host = {'Memory': 4294967296, 'MemorySwap': 4294967296, 'PidsLimit': 256, 'NanoCpus': 2000000000,
                'ReadonlyRootfs': True, 'Privileged': False, 'NetworkMode': 'none', 'PortBindings': {}, 'Devices': [],
                'CapAdd': [], 'CapDrop': ['ALL'], 'SecurityOpt': ['no-new-privileges'], 'PidMode': '', 'IpcMode': 'private',
                'Tmpfs': {'/tmp': 'rw,nosuid,nodev,size=67108864'}}
        mounts = [{'Destination': dst, 'Type': 'bind', 'Source': src, 'RW': rw} for dst, src, rw in
                  [('/workspace', '/source', False), ('/harness', '/harness', False), ('/durability-binding.json', '/input.json', False),
                   ('/durability-evidence', '/output', True)]]
        mounts += [{'Destination': '/' + name, 'Type': 'volume', 'Name': 'owned-' + name, 'RW': True} for name in ['cargo', 'target']]
        info = {'Image': record['image'], 'HostConfig': host, 'Config': {'User': '10001:10001'}, 'Mounts': mounts}
        M['verify_container'](info, record, source_root, harness, input_path, output_path)
        for key, value in [('Memory', 8589934592), ('MemorySwap', -1), ('PidsLimit', 512), ('NanoCpus', 0),
                           ('ReadonlyRootfs', False), ('Privileged', True), ('NetworkMode', 'bridge'), ('CapAdd', ['SYS_ADMIN'])]:
            bad = copy.deepcopy(info); bad['HostConfig'][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                M['verify_container'](bad, record, source_root, harness, input_path, output_path)
        for mutate in (lambda i: i.update(Image='sha256:' + 'd' * 64),
                       lambda i: i['Mounts'][0].update(RW=True), lambda i: i['Mounts'].pop(),
                       lambda i: i['Mounts'].append({'Destination': '/run/secrets', 'Type': 'volume', 'Name': 'credentials', 'RW': False})):
            bad = copy.deepcopy(info); mutate(bad)
            with self.assertRaises(ValueError): M['verify_container'](bad, record, source_root, harness, input_path, output_path)

    def test_guest_uses_qualified_nextest_and_compiler_with_verified_protoc_and_keeps_failed_streams(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); scratch = root / 'scratch'; output_dir = root / 'output'; output_dir.mkdir()
            tool = root / 'protoc'; tool.write_bytes(b'public-fake-tool')
            config = spec(); (root / 'input.json').write_bytes(M['encoded'](config))
            actual_coverage = runpy.run_path(str(ROOT / 'tools/coverage.py'))
            tools = dict(cargo_compiler='/qualified/rustc', cargo_rustdoc='/qualified/rustdoc', rustfmt_executable='/qualified/rustfmt',
                         ordinary_target='/target/qualified', native_target=config['target'], nextest_executable='/qualified/cargo-nextest',
                         test_executor=dict(environment_key='QUALIFIED_RUNNER', command='qualified-runner'))
            coverage = dict(actual_coverage, qualified_tools=lambda: tools, nextest_policy=lambda: None)
            actual_run_path = runpy.run_path
            def route(path):
                return coverage if str(path).endswith('tools/coverage.py') or str(path).endswith('tools\\coverage.py') else actual_run_path(path)
            def paths(value):
                return {'/durability-binding.json': root / 'input.json', '/durability-evidence': output_dir,
                        '/target/native-sample-scratch': scratch}.get(str(value), Path(value))
            captured = []
            def command(argv, output, **kwargs):
                env = kwargs['env']; captured.append(argv)
                self.assertEqual(argv[0], '/qualified/cargo-nextest')
                self.assertEqual(env['PROTOC'], str(tool)); self.assertEqual(env['RUSTC'], '/qualified/rustc')
                self.assertEqual(env['RUSTC_BOOTSTRAP'], '-1'); self.assertEqual(env['HSEARCH_DURABILITY_STRICT_SAMPLE'], '1')
                self.assertIsNone(json.loads(env['HSEARCH_DURABILITY_BINDING'])['nextest_run_id'])
                self.assertTrue(kwargs['capture_both'])
                return 101, {'stdout': b'bounded stdout', 'stderr': b'bounded failure'}
            with patch.dict(G, Path=paths), patch.object(G['runpy'], 'run_path', side_effect=route), \
                 patch.dict(G['os'].environ, {}, clear=True):
                result = M['guest_sample'](SimpleNamespace(write=lambda raw: None, emit=lambda row: None), {'PROTOC': str(tool)},
                                           {'tool_actual_sha256': M['digest'](tool.read_bytes())}, command)
            self.assertEqual(result, 101)
            self.assertEqual((output_dir / 'stderr.log').read_bytes(), b'bounded failure')
            self.assertEqual(len(captured), 1)

    def test_export_refuses_unbounded_extra_missing_and_wrong_binding_before_oracle(self):
        for kind in ('missing', 'extra', 'oversize', 'binding'):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                for name in ('stdout.log', 'stderr.log'): (root / name).write_bytes(b'')
                (root / 'command.json').write_bytes(b'{"exit_code":0}')
                if kind != 'missing': (root / 'receipt.json').write_bytes(M['encoded']({'binding': dict(spec(), source_head='d' * 40)}))
                if kind == 'extra': (root / 'unlisted').write_bytes(b'x')
                if kind == 'oversize': (root / 'stdout.log').write_bytes(b'x' * (M['CAP'] + 1))
                with self.assertRaises((ValueError, KeyError)): M['verify_export'](root, spec())

    def test_host_checks_actual_packed_bytes_stream_identity_and_full40_before_acceptance(self):
        bundle, verifier = M['modules']()
        tapes = runpy.run_path(str(ROOT / 'crates/testkit/research/test_transaction_witness.py'))
        tape = tapes['history']('raft-rs')
        files = dict(tape.files, **{'artifacts/events-000.ndjson': b''.join(M['encoded'](r) for r in tape.rows)})
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); scratch = root / 'scratch'; scratch.mkdir()
            reports = []
            for ordinal in range(40):
                directory = scratch / ('hsearch-durability-raft-rs-1-2-%d' % ordinal); directory.mkdir()
                for name, raw in files.items():
                    path = directory / name; path.parent.mkdir(exist_ok=True); path.write_bytes(raw)
                reports.append({'directory': str(directory), 'counts': {}})
            expected = spec(); actual = dict(expected, nextest_run_id='00000000-0000-0000-0000-000000000001',
                                              protoc_sha256=M['PROTOC'][expected['target']])
            runtime = {'strict': True, 'binding': actual, 'uid': 10001, 'gid': 10001, 'architecture': 'x86_64', 'scope': 'Synthetic HOST fixture',
                       'caps': {'memory.max': '4294967296', 'memory.swap.max': '0', 'pids.max': '256', 'cpu.max': '200000 100000'}}
            sample = {'sample_schema': 2, 'candidate': 'raft-rs', 'profile': 'dev', 'elapsed_us': 1000, 'runtime': runtime,
                      'workloads': [{'raw': r} for r in reports[:18]], 'failure': reports[18], 'aggregate_counts': {},
                      'scales': [{'groups': 1, 'raw_groups': reports[19:20]}, {'groups': 4, 'raw_groups': reports[20:24]},
                                 {'groups': 16, 'raw_groups': reports[24:40]}]}
            packed = root / 'packed'; receipt = bundle['collect'](sample, scratch, packed, actual)
            (packed / 'receipt.json').write_bytes(M['encoded'](receipt))
            (packed / 'stdout.log').write_bytes(b'NATIVE_DURABILITY_SAMPLE: ' + M['encoded'](sample))
            (packed / 'stderr.log').write_bytes(b'')
            (packed / 'command.json').write_bytes(M['encoded']({'argv': M['sample_command'](M['matrix'](actual['target'])[0]), 'exit_code': 0}))
            proof = {'native_complete_sample': 'pass', 'temporal_application': {'contexts': 40, 'context_ids':M['context_ids']()},
                     'unsafe_controls': {'actual_artifact_negative_controls': 'pass', 'controls': [{'control':name,'result':'refused'} for name in M['UNSAFE_IDS']]}}
            with patch.dict(G, modules=lambda: (bundle, dict(verifier, verify=lambda *a: proof))):
                result = M['verify_export'](packed, expected)
                self.assertEqual(result['full40_unrelated_sync']['count'], 40)
                self.assertEqual(result['binding'], actual)
                payload = packed / 'payload-000.bin'; original = payload.read_bytes(); payload.write_bytes(b'x' + original[1:])
                with self.assertRaises(ValueError): M['verify_export'](packed, expected)
                payload.write_bytes(original)
                changed = copy.deepcopy(sample); changed['elapsed_us'] += 1
                (packed / 'stdout.log').write_bytes(b'NATIVE_DURABILITY_SAMPLE: ' + M['encoded'](changed))
                with self.assertRaisesRegex(ValueError, 'Packed sample differs'): M['verify_export'](packed, expected)

    def test_acknowledged_stopped_cid_is_verified_before_removal_and_failure_keeps_lease(self):
        sm = runpy.run_path(str(ROOT / 'tools/sandbox.py')); gib = sm['GIB']
        capacity = {'cpu_millis': 8000, 'memory_bytes': 16 * gib, 'pids': 2048, 'disk_bytes': 40 * gib}
        for fail in (False, True, "checkpoint"):
            with self.subTest(fail=fail), tempfile.TemporaryDirectory() as temporary:
                events = []; namespace = 'hsearch-m02-10-0123456789ab'
                class Docker:
                    engine = 'engine'; info = {'Architecture': 'x86_64'}
                    def __init__(self): self.capacity = capacity; self.container = None
                    def command(self, *args, **kwargs):
                        events.append(args[0])
                        if args[0] == 'create':
                            self.container = {'Id': 'a' * 64, 'Image': spec()['image'], 'Config': {'Labels': {sm['LABEL']: 'mine', 'io.hsearch.namespace': namespace}},
                                              'State': {'Status': 'created', 'Running': False, 'ExitCode': 0}}
                            return 'a' * 64
                        if args[0] == 'start': self.container['State'].update(Status='running', Running=True)
                        if args[0] == 'wait':
                            self.container['State'].update(Status='exited', Running=False); return '0'
                        if args[0] == 'logs':
                            self.assert_bounded = kwargs['output_bytes'] <= 1024**2; return {'stdout': b'bounded log', 'stderr': b''}
                        if args[:2] == ('container', 'rm'): self.container = None
                        return ''
                    def inspect(self, kind, names):
                        if kind == 'container': return [self.container]
                        return [{'Name': n, 'Labels': {sm['LABEL']: 'mine', 'io.hsearch.namespace': namespace}} for n in names]
                docker = Docker(); sandbox = sm['Sandbox'](Path(temporary), docker=docker, free_disk=lambda: 100 * gib)
                record = dict(namespace=namespace, owner='mine', source=str(ROOT), engine='engine', phase='ready',
                              image=spec()['image'], snapshot_schema=2, resource_protocol=2)
                sandbox.registry.acquire(namespace, sm['STORAGE'], capacity, 'engine', record)
                folder = sandbox.snapshot(record); folder.mkdir(parents=True)
                raw = M['encoded'](spec()); (folder / 'durability-binding.json').write_bytes(raw)
                sandbox.registry.update(namespace, native_durability_input={'bytes': len(raw), 'sha256': M['digest'](raw)})
                sandbox.harness = lambda r: folder / 'harness'
                sandbox.backend_free = lambda *a, **kw: 100 * gib
                def verify(*args):
                    events.append('independent-verify')
                    self.assertIsNotNone(docker.container)
                    self.assertEqual(docker.container['State']['Status'], 'exited')
                    if fail is True: raise ValueError('bad raw artifact')
                    return {'binding': dict(spec(), nextest_run_id='00000000-0000-0000-0000-000000000001')}
                native = dict(M, verify_container=lambda *a: events.append('inspect-caps'), verify_export=verify,
                              sync_file=lambda *a: events.append('sync-file'), sync_directory=lambda *a: events.append('sync-directory'))
                options = checkpoint_options()
                if fail == 'checkpoint':
                    def refuse_checkpoint(*args):
                        events.append('checkpoint-refused')
                        raise OSError('synthetic journal failure')
                    options['checkpoint'] = refuse_checkpoint
                with patch.object(sm['runpy'], 'run_path', return_value=native):
                    if fail == 'checkpoint':
                        with self.assertRaisesRegex(OSError, 'journal failure'):
                            sandbox.run(namespace, 'durability-sample', artifact_directory=folder / 'durability-evidence', stream=False, **options)
                        self.assertEqual(events.count('checkpoint-refused'), 1)
                    elif fail:
                        with self.assertRaisesRegex(ValueError, 'bad raw artifact'):
                            sandbox.run(namespace, 'durability-sample', artifact_directory=folder / 'durability-evidence', stream=False, **options)
                    else:
                        result = sandbox.run(namespace, 'durability-sample', artifact_directory=folder / 'durability-evidence', stream=False, **options)
                        self.assertEqual(result['exit_code'], 0)
                self.assertTrue(docker.assert_bounded)
                with sandbox.registry.edit() as registry:
                    children = [r for r in registry['leases'].values() if r.get('parent') == namespace]
                if fail:
                    self.assertIsNotNone(docker.container); self.assertEqual(len(children), 1)
                    self.assertFalse(children[0]['owner_active']); self.assertFalse(children[0]['native_export_verified'])
                else:
                    self.assertIsNone(docker.container); self.assertEqual(children, [])
                    self.assertLess(events.index('independent-verify'), events.index('container'))

    def test_public_copy_is_explicit_bounded_and_never_traverses_registry(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve(); src = root / 'src'; src.mkdir()
            (src / 'stdout.log').write_bytes(b''); (src / 'receipt.json').write_bytes(b'{}')
            M['publish_export'](src, root / 'public')
            self.assertEqual((root / 'public/receipt.json').read_bytes(), b'{}')
            with self.assertRaises(ValueError): M['publish_export'](src, root / 'public')
            (src / 'private.key').write_bytes(b'not-a-secret-fixture')
            with self.assertRaises(ValueError): M['publish_export'](src, root / 'refused')
            self.assertFalse((root / 'refused').exists())


if __name__ == '__main__':
    unittest.main()
