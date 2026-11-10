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
        self.record = {'worktree': str(source), 'packet': {'task_id': 'M02.10', 'research_campaign': 'durability-v1'}, 'evidence': []}
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
        context = {'claim': 'claim', 'source': store.fingerprint(None), 'container_id': 'c'*64,
                   'terminal': {'running': False, 'exit_code': 0}}
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
