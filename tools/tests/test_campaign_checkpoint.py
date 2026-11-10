"""HOST failure controls; mocked directory sync is not physical durability proof."""
from contextlib import contextmanager
import copy
import json
from pathlib import Path
import runpy
import tempfile
import time
import unittest
from unittest.mock import patch

M = runpy.run_path(str(Path(__file__).resolve().parents[1] / 'campaign_checkpoint.py'))
G = M['Journal'].terminal.__globals__


class Store:
    def __init__(self, root):
        self.root = root / 'state'; self.root.mkdir()
        self.path = self.root / 'tasks.json'
        source = root / 'source'; source.mkdir()
        self.record = {'worktree': str(source), 'packet': {'task_id': 'M02.10', 'research_campaign': 'durability-v1', 'output_budget_bytes':1024**2}, 'evidence': []}
        self.events = []
        self.save()

    def save(self): self.path.write_bytes(M['encoded']({'tasks': {'claim': self.record}}))
    def get(self, claim, owner): return copy.deepcopy(self.record)
    def fingerprint(self, record): return {'head': 'a'*40, 'digest': 'b'*64, 'clean': True}
    def append(self, claim, owner, kind, value):
        self.events.append('journal')
        raw = M['encoded'](value); h = M['digest'](raw)
        folder = self.root / 'evidence' / claim; folder.mkdir(parents=True, exist_ok=True)
        (folder / (h+'.json')).write_bytes(raw)
        self.record['evidence'].append({'kind': kind, 'sha256': h, 'bytes': len(raw)})
        self.save()
        return h
    @contextmanager
    def edit(self):
        yield {'tasks': {'claim': self.record}}
        self.save()


class CheckpointControls(unittest.TestCase):
    def fixture(self, root):
        store = Store(root); output = root / 'public'; output.mkdir()
        journal = M['Journal'](store, 'claim', 'owner', store.fingerprint(None), output, time.monotonic()+10000)
        context = {'mode': 'durability-sample', 'claim': 'claim', 'source': store.fingerprint(None), 'container_id': 'c'*64,
                   'terminal': {'running': False, 'exit_code': 0},
                   'packet_sha256':'d'*64, 'image_inputs_sha256':'e'*64,'image':'sha256:'+'f'*64,
                   'config_sha256':'1'*64, 'namespace':'hsearch-m02-10-'+'2'*12,
                   'run_lease':'hsearch-run-'+'3'*12, 'harness_hashes':{'execute.py':'4'*64},
                   'budget':{'cpu_millis':2000,'memory_bytes':4*1024**3,'pids':256,'disk_bytes':8*1024**3},
                   'command_budget':{'maximum_calls':11,'consumed_calls':9}}
        result = {'container_id': 'c'*64, 'exit_code': 0, 'truncated': False, 'output': 'public',
                  'raw_streams': {'stdout': b'public\x00\xff', 'stderr': b'warning\n'}}
        return store, journal, context, result

    def syncs(self, store):
        def sync_file(path): store.events.append('file:'+path.name)
        def sync_directory(path): store.events.append('directory:'+path.name)
        return patch.dict(G, sync_file=sync_file, sync_directory=sync_directory)

    def test_raw_bytes_export_and_compact_journal_precede_disposal(self):
        with tempfile.TemporaryDirectory() as t:
            root=Path(t).resolve(); store,journal,context,result=self.fixture(root)
            export=root/'export'; export.mkdir(); payload=b'{"public":true}'; (export/'index.json').write_bytes(payload)
            result['export_files']={'index.json': {'bytes':len(payload),'sha256':M['digest'](payload)}}
            with self.syncs(store):
                ref=journal.terminal(context,result,export)
            store.events.append('dispose')
            self.assertLess(store.events.index('directory:attempt-00'),store.events.index('journal'))
            self.assertLess(store.events.index('file:tasks.json'),store.events.index('dispose'))
            self.assertEqual(journal.verify(ref),ref)
            manifest=json.loads((journal.output/'attempt-00/manifest.json').read_bytes())
            self.assertEqual(manifest['logs']['stdout']['sha256'],M['digest'](b'public\x00\xff'))
            self.assertLess(len(M['encoded'](ref)),16384)

    def test_journal_failure_leaves_orphan_bytes_and_never_returns_disposal_reference(self):
        with tempfile.TemporaryDirectory() as t:
            store,journal,context,result=self.fixture(Path(t).resolve())
            with self.syncs(store),patch.object(store,'append',side_effect=OSError('synthetic journal failure')):
                with self.assertRaises(OSError):journal.terminal(context,result)
            self.assertTrue((journal.output/'attempt-00/manifest.json').exists())
            self.assertEqual(store.record['evidence'],[])
            self.assertGreater(journal.retained()[0],0)

    def test_directory_or_registry_sync_failure_retains_evidence_without_grant(self):
        for fail in ('directory','registry'):
            with self.subTest(fail=fail),tempfile.TemporaryDirectory() as t:
                store,journal,context,result=self.fixture(Path(t).resolve())
                def fsync(path):
                    if fail=='registry' and path==store.path:raise OSError('registry sync failure')
                def dsync(path):
                    if fail=='directory':raise OSError('directory sync failure')
                with patch.dict(G,sync_file=fsync,sync_directory=dsync),self.assertRaises(OSError):journal.terminal(context,result)
                self.assertEqual(journal.events,[])
                self.assertTrue((journal.output/'attempt-00/manifest.json').exists())

    def test_public_mutation_or_lost_journal_refuses_reuse(self):
        for failure in ('file','journal','extra'):
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as t:
                store,journal,context,result=self.fixture(Path(t).resolve())
                with self.syncs(store):ref=journal.terminal(context,result)
                if failure=='file':(journal.output/'attempt-00/proof.json').write_bytes(b'{}')
                if failure=='extra':(journal.output/'attempt-00/orphan.bin').write_bytes(b'x')
                if failure=='journal':store.record['evidence']=[];store.save()
                with self.assertRaises(ValueError):journal.verify(ref)

    def test_late_stage_failure_preserves_independently_readable_success_prefix(self):
        with tempfile.TemporaryDirectory() as t:
            store,journal,context,result=self.fixture(Path(t).resolve())
            with self.syncs(store):first=journal.terminal(context,result)
            failed=copy.deepcopy(result); failed.update(exit_code=7,verification_failed=True)
            context=copy.deepcopy(context);context['terminal']['exit_code']=7
            with self.syncs(store):second=journal.terminal(context,failed)
            self.assertEqual(journal.verify(first)['status'],'verified')
            self.assertEqual(journal.verify(second)['status'],'failed-unqualified')

    def test_unrelated_terminal_source_and_raw_bounds_refuse_before_publication(self):
        for failure in ('source','cid','live','bytes'):
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as t:
                store,journal,context,result=self.fixture(Path(t).resolve())
                if failure=='source':context['source']={}
                if failure=='cid':result['container_id']='d'*64
                if failure=='live':context['terminal']['running']=True
                if failure=='bytes':result['raw_streams']['stdout']=b'x'*(512*1024+1)
                with self.syncs(store),self.assertRaises(ValueError):journal.terminal(context,result)
                self.assertEqual(store.record['evidence'],[])

    def test_orphan_size_type_and_count_remain_charged_before_next_allocation(self):
        for failure in ('size','directory','count'):
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as t:
                store,journal,context,result=self.fixture(Path(t).resolve())
                if failure=='count':
                    for i in range(28):(journal.output/('attempt-%02d'%i)).mkdir()
                else:
                    folder=journal.output/'attempt-00';folder.mkdir()
                    if failure=='size':(folder/'orphan.bin').write_bytes(b'x'*(M['CAP']+1))
                    else:(folder/'nested').mkdir()
                with self.syncs(store),self.assertRaises(ValueError):journal.terminal(context,result)
                self.assertEqual(store.record['evidence'],[])

    def test_expired_deadline_cannot_start_or_publish_a_worker_event(self):
        with tempfile.TemporaryDirectory() as t:
            store,journal,context,result=self.fixture(Path(t).resolve())
            journal.deadline=M['Deadline'](time.monotonic()+599)
            with self.assertRaisesRegex(ValueError,'allowance'):journal.terminal(context,result)
            self.assertEqual(list(journal.output.iterdir()),[])
            with self.assertRaises(ValueError):M['Deadline'](float('nan'))


    def test_large_closed_object_inventory_uses_compact_manifest_and_is_self_contained(self):
        with tempfile.TemporaryDirectory() as t:
            root=Path(t).resolve();store,journal,context,result=self.fixture(root)
            context['mode']='durability-object';context['budget']['disk_bytes']=18*1024**3
            export=root/'export';export.mkdir();pins={}
            names=['index.json','run.jsonl','faults.jsonl']+['case-%03d.jsonl'%i for i in range(144)]
            for name in names:
                raw=b'{"fixture":"public"}';(export/name).write_bytes(raw)
                pins[name]={'bytes':len(raw),'sha256':M['digest'](raw)}
            result['export_files']=pins
            with self.syncs(store):reference=journal.terminal(context,result,export)
            self.assertLess((journal.output/'attempt-00/manifest.json').stat().st_size,16384)
            self.assertEqual(json.loads((journal.output/'attempt-00/reference.json').read_bytes()),reference)
            self.assertEqual(journal.verify(reference),reference)

    def test_secret_or_unlisted_export_refuses_before_first_public_file(self):
        for failure in ('secret-log','secret-file','unknown-file','changed-export','missing-binding'):
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as t:
                root=Path(t).resolve();store,journal,context,result=self.fixture(root)
                export=root/'export';export.mkdir()
                raw=b'Authorization: synthetic-fixture' if failure=='secret-file' else b'{}'
                name='server.key' if failure=='unknown-file' else 'index.json'
                (export/name).write_bytes(raw)
                result['export_files']={name:{'bytes':len(raw),'sha256':M['digest'](raw)}}
                if failure=='secret-log':result['raw_streams']['stderr']=b'Authorization: synthetic-fixture'
                if failure=='changed-export':result['export_files'][name]['sha256']='0'*64
                if failure=='missing-binding':context.pop('harness_hashes')
                with self.syncs(store),self.assertRaises(ValueError):journal.terminal(context,result,export)
                self.assertEqual(list(journal.output.iterdir()),[])
                self.assertEqual(store.record['evidence'],[])

    def test_public_reference_and_actual_journal_blob_are_both_checked(self):
        for failure in ('public-reference','journal-blob','inventory'):
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as t:
                store,journal,context,result=self.fixture(Path(t).resolve())
                with self.syncs(store):reference=journal.terminal(context,result)
                if failure=='public-reference':path=journal.output/'attempt-00/reference.json'
                elif failure=='inventory':path=journal.output/'attempt-00/inventory.json'
                else:path=store.root/'evidence/claim'/(M['digest'](M['encoded'](reference))+'.json')
                path.write_bytes(b'{}')
                with self.assertRaises(ValueError):journal.verify(reference)


    def test_task_reserve_counts_registered_and_physical_orphan_storage(self):
        for failure in ('physical','records'):
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as t:
                store,journal,context,result=self.fixture(Path(t).resolve())
                journal.admit_task_reserve()
                if failure=='physical':
                    folder=store.root/'evidence/claim';folder.mkdir(parents=True)
                    for i in range(41):
                        with (folder/('%064x.json'%i)).open('xb') as handle:handle.truncate(1024**2)
                else:store.record['evidence']=[{'bytes':1} for _ in range(857)]
                with self.assertRaisesRegex(ValueError,'existing task quota'):journal.admit_task_reserve()

    def test_full_public_plan_overflow_refuses_before_first_destination_file(self):
        with tempfile.TemporaryDirectory() as t:
            root=Path(t).resolve();store,journal,context,result=self.fixture(root)
            export=root/'export';export.mkdir();raw=b'{}';(export/'index.json').write_bytes(raw)
            result['export_files']={'index.json':{'bytes':2,'sha256':M['digest'](raw)}}
            with self.syncs(store),patch.dict(G,EXPORT=64),self.assertRaisesRegex(ValueError,'plan exceeds bound'):
                journal.terminal(context,result,export)
            self.assertEqual(list(journal.output.iterdir()),[])
            self.assertEqual(store.record['evidence'],[])


class WorkerCallBudgetControls(unittest.TestCase):
    def setUp(self):
        self.api=runpy.run_path(str(Path(__file__).resolve().parents[1]/'sandbox.py'))

    def test_fixed_budget_charges_each_call_and_refuses_unlisted_or_duplicate_operation(self):
        from types import SimpleNamespace
        requirements=[];calls=[]
        docker=SimpleNamespace(command=lambda *a,**kw:calls.append(a),inspect=lambda *a:calls.append(a))
        clock=SimpleNamespace(require=requirements.append)
        budget=self.api['CampaignRunCalls'](docker,'durability-object',clock)
        self.assertEqual(budget.receipt()['maximum_calls'],18)
        budget.command('create','fixed-worker')
        self.assertEqual(calls,[('create','fixed-worker')])
        for operation in [('create','duplicate'),('exec','arbitrary'),('container','rm','unknown')]:
            if operation[:2]==('container','rm'):
                budget.command(*operation)
            with self.assertRaises(self.api['Refused']):budget.command(*operation)
        self.assertEqual(len(calls),2)
        self.assertGreater(requirements[0],1250+600)
        budget.checkpoint_complete()
        self.assertEqual(budget.receipt()['checkpoint_allowance_seconds'],0)

    def test_timeout_and_deadline_refuse_before_backend_call(self):
        from types import SimpleNamespace
        backend=[];clock=SimpleNamespace(require=lambda seconds:None)
        budget=self.api['CampaignRunCalls'](SimpleNamespace(command=lambda *a,**k:backend.append(a)),'prefetch',clock)
        with self.assertRaises(self.api['Refused']):budget.command('wait','cid',timeout=1251)
        self.assertEqual(backend,[])
        def refuse(seconds):raise ValueError('clock exhausted')
        clock.require=refuse
        with self.assertRaises(ValueError):budget.command('wait','cid',timeout=1250)
        self.assertEqual(backend,[])


class OrchestrationClockControls(unittest.TestCase):
    def test_setup_and_teardown_use_shared_deadline_and_charge_before_backend(self):
        from types import SimpleNamespace
        api=runpy.run_path(str(Path(__file__).resolve().parents[1]/'sandbox.py'));events=[]
        docker=SimpleNamespace(command=lambda *a,**kw:events.append(('backend',a)),inspect=lambda *a:events.append(('inspect',a)))
        clock=SimpleNamespace(require=lambda seconds:events.append(('deadline',seconds)))
        wrapped=api['CampaignDockerCalls'](docker,clock,lambda:events.append(('charge',)))
        wrapped.command('compose','--project-name','owned','down',timeout=150)
        self.assertEqual(events[:2],[('deadline',750),('charge',)])
        before=len(events)
        for args in [('exec','unbudgeted'),('build','too-long')]:
            with self.assertRaises(api['Refused']):wrapped.command(*args,timeout=601)
        self.assertEqual(len(events),before)
        def expire(seconds):raise ValueError('shared clock exhausted')
        clock.require=expire
        with self.assertRaises(ValueError):wrapped.inspect('volume',['owned'])
        self.assertEqual(len(events),before)
