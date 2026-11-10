"""Small synthetic codec/IO controls. No native process or retained campaign data."""
import copy
import json
from pathlib import Path
import runpy
import unittest

C = runpy.run_path(str(Path(__file__).with_name('native_commit_verify.py')))
F = runpy.run_path(str(Path(__file__).with_name('native_failure_verify.py')))
V = C['V']


def encoded(value):
    return json.dumps(value, separators=(',', ':')).encode()


def frame(value):
    body = encoded(value)
    return b'HSRCHD01' + len(body).to_bytes(8, 'little') + body


def literal(previous, raw):
    return (b'HSRCHW01' + len(previous).to_bytes(4, 'little') + len(raw).to_bytes(4, 'little') +
            b'\1' + len(raw).to_bytes(4, 'little') + raw)


def varint(value):
    result = bytearray()
    while value > 127:
        result.append((value & 127) | 128); value >>= 7
    result.append(value)
    return bytes(result)


def proto(*fields):
    result = b''
    for field, value in fields:
        result += varint(field * 8 + (2 if type(value) is bytes else 0))
        result += varint(len(value)) + value if type(value) is bytes else varint(value)
    return result


def operation(identity='synthetic'):
    prefix = f'id={identity};tenant=0;key=0;stream=0;'
    return {'id':identity, 'tenant':0, 'key':0, 'stream':0, 'payload':prefix + '#' * (1024-len(prefix))}


MEMBERSHIP = {'configs':[[1,2,3]], 'nodes':{'1':None,'2':None,'3':None}}
CONF = proto((1, b'\1\2\3'))


def log_id(index):
    return {'leader_id':{'term':1,'node_id':1},'index':index}


def open_entry(index, op=None):
    return {'log_id':log_id(index), 'payload':{'Membership':MEMBERSHIP} if index == 0 else
            {'Normal':{'operations':[op or operation()]}}}


def raft_entry(index=1, op=None):
    return {'kind':0,'term':1,'index':index,'data':{'Utf8':encoded({'operations':[op or operation()]}).decode()},
            'context':[],'sync_log':False}


def raft_state(entries=(), applied=0, op=None, snapshot=b''):
    return {'schema_version':2,'hard_state':list(proto((1,1))), 'entries':list(entries),
            'conf_state':list(CONF), 'snapshot':list(snapshot),
            'application':{'applied':applied,'operations':{} if not applied else {(op or operation())['id']:op or operation()}}}


def open_log(entries=()):
    return {'vote':{'leader_id':{'term':1,'node_id':1},'committed':True},'purged':None,
            'committed':None,'entries':{str(e['log_id']['index']):e for e in entries}}


def machine(index=-1, op=None):
    return {'applied':{'last':None if index < 0 else log_id(index),
            'membership':{'log_id':None if index < 0 else log_id(0),
                          'membership':{'configs':[],'nodes':{}} if index < 0 else copy.deepcopy(MEMBERSHIP)},
            'operations':{} if index < 1 else {(op or operation())['id']:op or operation()}}, 'snapshot':None}


class Tape:
    def __init__(self):
        self.rows, self.data, self.disks = [], bytearray(), {}

    def emit(self, kind, **values):
        assert len(self.rows) < 4096
        row = dict(kind=kind, sequence=len(self.rows), **copy.deepcopy(values))
        self.rows.append(row)
        return row

    def extent(self, raw):
        assert len(self.data) + len(raw) <= 1048576
        value = {'file':'wire-000.bin','offset':len(self.data),'bytes':len(raw)}
        self.data.extend(raw)
        return value

    @property
    def files(self):
        return {'artifacts/wire-000.bin':bytes(self.data)}

    def create(self, name, value):
        self.disks[name] = {'incarnation':1,'id':0,'completed':0,'raw':b'','token':None}
        self.emit('disk_open',directory=name,incarnation=1,predecessor=0)
        return self.complete(name,value)

    def begin(self, name):
        disk = self.disks[name]
        disk['id'] += 1
        token = dict(disk=name,incarnation=disk['incarnation'],id=disk['id'],predecessor=disk['completed'])
        self.emit('disk_transaction',directory=name,transaction=token)
        return token

    def io(self, kind, token, **values):
        return self.emit(kind,directory=token['disk'],transaction=token,**values)

    def complete(self, name, value):
        raw, disk = frame(value), self.disks[name]
        token = self.begin(name)
        self.io('file_write_attempt',token)
        self.io('file_write',token,bytes=len(raw))
        self.io('sync_attempt',token,target='file')
        self.io('sync_complete',token,target='file')
        self.io('rename_complete',token)
        self.io('sync_attempt',token,target='directory')
        self.io('sync_complete',token,target='directory')
        app = value.get('application',value.get('applied'))
        summary = None
        if app is not None:
            summary = dict(candidate='raft-rs' if 'application' in value else 'openraft',ids=sorted(app['operations']))
            summary['index' if 'application' in value else 'log_id'] = app.get('applied',app.get('last'))
        self.io('disk_persisted',token,wire=self.extent(literal(disk['raw'],raw)),bytes=len(raw),
                file_sync=True,directory_sync=True,application=summary)
        disk.update(raw=raw,completed=token['id'],token=token)
        return token

    def reopen(self, name):
        disk = self.disks[name];disk['incarnation'] += 1
        delta = (b'HSRCHW01' + len(disk['raw']).to_bytes(4,'little') * 2 + b'\0' +
                 bytes(4) + len(disk['raw']).to_bytes(4,'little'))
        self.emit('disk_reopened',directory=name,incarnation=disk['incarnation'],
                  predecessor=disk['completed'],wire=self.extent(delta))

    def wire(self, candidate, entries):
        if candidate == 'openraft':
            raw = encoded({'vote':{'leader_id':{'term':1,'node_id':1},'committed':True},
                           'prev_log_id':None,'entries':entries,'leader_commit':None})
        else:
            raw_entries = [proto((1,e['kind']),(2,e['term']),(3,e['index']),
                                 (4,e['data']['Utf8'].encode())) for e in entries]
            raw = proto((1,3),(2,2),(3,1),*((7,e) for e in raw_entries))
        edge = sum(row['kind']=='wire' for row in self.rows)+1
        self.emit('wire',edge=edge,source=1,target=2,dropped=False,
                  message='append_request' if candidate=='openraft' else 'MsgAppend',wire=self.extent(raw))
        if candidate=='raft-rs':self.emit('step_received',edge=edge,node=2)


def ready(tape, node, entries=(), committed=(), atomic=()):
    tape.emit('ready_persisted',node=node,transaction=tape.disks[f'node-{node}']['token'],
              entries=[e['index'] for e in entries],entry_terms=[e['term'] for e in entries],
              committed_entries=list(committed),persisted_snapshot=None,snapshot=False,hard_state=False,
              atomic_application_indices=list(atomic))


def append(tape, node, entries):
    token = tape.disks[f'node-{node}-log']['token']
    tape.emit('append_readable_and_persisted',node=node,transaction=token,
              indices=[e['log_id']['index'] for e in entries],log_ids=[e['log_id'] for e in entries],purged=None)
    tape.emit('flush_callback_success',node=node,transaction=token,after_actual_sync=True)


def history(candidate, *, light=False):
    tape = Tape()
    if candidate=='raft-rs':
        for node in (1,2):tape.create(f'node-{node}',raft_state())
        entry = raft_entry()
        tape.complete('node-1',raft_state([entry]));ready(tape,1,[entry])
        tape.emit('advance_append',node=1,light_committed_entries=[])
        tape.wire(candidate,[entry])
        tape.complete('node-2',raft_state([entry]));ready(tape,2,[entry])
        tape.emit('advance_append',node=2,light_committed_entries=[])
        if light:
            ready(tape,1)
            tape.emit('advance_append',node=1,light_committed_entries=[[1,1]])
        else:tape.emit('committed_apply_received',node=1,entries=[[1,1]])
        token=tape.complete('node-1',raft_state([entry],1))
        if not light:
            ready(tape,1,committed=[[1,1]],atomic=[1])
            tape.emit('advance_append',node=1,light_committed_entries=[])
        tape.emit('applied',node=1,index=1,term=1,ids=['synthetic'],transaction=token)
    else:
        for node in (1,2):
            tape.create(f'node-{node}-log',open_log());tape.create(f'node-{node}-machine',machine())
        for index in (0,1):
            entries=[open_entry(i) for i in range(index+1)]
            tape.complete('node-1-log',open_log(entries));append(tape,1,[entries[-1]])
            tape.wire(candidate,[entries[-1]])
            tape.complete('node-2-log',open_log(entries));append(tape,2,[entries[-1]])
            tape.emit('committed_apply_received',node=1,log_ids=[log_id(index)])
            token=tape.complete('node-1-machine',machine(index))
            tape.emit('applied',node=1,index=index,log_id=log_id(index),
                      ids=['synthetic'] if index else [],transaction=token)
    return tape


class TransactionTests(unittest.TestCase):
    def check_history(self, tape, candidate):
        return F['order_events'](tape.rows,candidate,tape.files)

    def ledger(self, tape, rows=None):
        ledger=V['Transactions'](tape.files)
        for row in tape.rows if rows is None else rows:ledger.observe(row)
        ledger.finish()
        return ledger

    def test_both_codecs_positive_and_ready_lightready(self):
        for candidate,light in (('openraft',False),('raft-rs',False),('raft-rs',True)):
            with self.subTest(candidate=candidate,light=light):
                result=self.check_history(history(candidate,light=light),candidate)
                self.assertEqual(result['temporal_committed_application'],'pass')

    def test_unrelated_sync_cannot_install_declared_entry(self):
        for candidate in ('openraft','raft-rs'):
            tape=history(candidate)
            # Move the entry's IO completion beyond its declaration. An earlier
            # successful empty sync remains, exactly the dangerous authority gap.
            name='node-1-log' if candidate=='openraft' else 'node-1'
            start=next(i for i,r in enumerate(tape.rows) if r['kind']=='disk_transaction' and
                       r['directory']==name and r['transaction']['id']==2)
            stop=next(i for i in range(start,len(tape.rows)) if tape.rows[i]['kind']=='disk_persisted')+1
            transaction=tape.rows[start:stop];del tape.rows[start:stop]
            tape.rows.extend(transaction)
            for i,row in enumerate(tape.rows):row['sequence']=i
            with self.subTest(candidate=candidate),self.assertRaises(ValueError):self.check_history(tape,candidate)

    def test_callback_only_without_prior_durable_quorum(self):
        for candidate in ('openraft','raft-rs'):
            tape=history(candidate)
            name='node-2-log' if candidate=='openraft' else 'node-2'
            rows=[r for r in tape.rows if r.get('directory')!=name and r.get('node')!=2]
            with self.subTest(candidate=candidate),self.assertRaises(ValueError):
                F['order_events'](rows,candidate,tape.files)

    def test_completed_unrelated_sync_with_valid_token_is_not_entry_content(self):
        # The transaction is complete, newer and correctly associated. Only
        # inspecting its contents distinguishes this from genuine persistence.
        for candidate in ('openraft','raft-rs'):
            tape=Tape()
            if candidate=='openraft':
                for node in (1,2):
                    tape.create(f'node-{node}-log',open_log());tape.create(f'node-{node}-machine',machine())
                entry=open_entry(0)
                tape.complete('node-1-log',open_log());append(tape,1,[entry])
                tape.wire(candidate,[entry])
                tape.complete('node-2-log',open_log([entry]));append(tape,2,[entry])
                tape.emit('committed_apply_received',node=1,log_ids=[log_id(0)])
                token=tape.complete('node-1-machine',machine(0))
                tape.emit('applied',node=1,index=0,log_id=log_id(0),ids=[],transaction=token)
                tape.complete('node-1-log',open_log([entry]))
                reason='Append declared entries/purge absent from completed contents'
            else:
                for node in (1,2):tape.create(f'node-{node}',raft_state())
                entry=raft_entry()
                tape.complete('node-1',raft_state());ready(tape,1,[entry])
                tape.emit('advance_append',node=1,light_committed_entries=[])
                tape.wire(candidate,[entry])
                tape.complete('node-2',raft_state([entry]));ready(tape,2,[entry])
                tape.emit('advance_append',node=2,light_committed_entries=[])
                tape.emit('committed_apply_received',node=1,entries=[[1,1]])
                token=tape.complete('node-1',raft_state([entry],1))
                ready(tape,1,committed=[[1,1]],atomic=[1])
                tape.emit('advance_append',node=1,light_committed_entries=[])
                tape.emit('applied',node=1,index=1,term=1,ids=['synthetic'],transaction=token)
                reason='Ready declared entries absent from completed contents'
            self.ledger(tape)  # Every physical transaction is structurally valid.
            with self.subTest(candidate=candidate),self.assertRaisesRegex(ValueError,reason):
                self.check_history(tape,candidate)

    def test_wrong_body_at_same_index_and_term(self):
        for candidate in ('openraft','raft-rs'):
            tape=history(candidate)
            # Change actual completed contents, keeping all transaction/wire
            # coordinates consistent. Identity alone must not certify the body.
            for row in tape.rows:
                if row['kind']!='disk_persisted':continue
                loc=row['wire'];offset=loc['offset'];raw=bytes(tape.data[offset:offset+loc['bytes']])
                raw=raw.replace(b'synthetic',b'different')
                tape.data[offset:offset+len(raw)]=raw
            with self.subTest(candidate=candidate),self.assertRaises(ValueError):self.check_history(tape,candidate)

    def test_missing_reordered_and_partial_io_completion(self):
        tape=Tape();tape.create('node-1',{'value':1})
        for kind in ('file_write_attempt','file_write','sync_attempt','sync_complete','rename_complete','disk_persisted'):
            rows=copy.deepcopy(tape.rows)
            rows.pop(next(i for i,r in enumerate(rows) if r['kind']==kind))
            with self.subTest(missing=kind),self.assertRaises(ValueError):self.ledger(tape,rows)
        rows=copy.deepcopy(tape.rows)
        a=next(i for i,r in enumerate(rows) if r['kind']=='rename_complete')
        rows[a],rows[a-1]=rows[a-1],rows[a]
        with self.assertRaises(ValueError):self.ledger(tape,rows)

    def test_failure_at_each_io_stage_cannot_complete(self):
        source=Tape();source.create('node-1',{'value':1})
        for cut in range(2,len(source.rows)-1):
            rows=copy.deepcopy(source.rows[:cut])
            token=source.rows[1]['transaction']
            rows.append(dict(kind='disk_error',directory='node-1',transaction=token,poisoned_until_reload=True))
            ledger=self.ledger(source,rows)
            self.assertEqual(ledger.disks['node-1']['raw'],b'')
            with self.subTest(stage=cut),self.assertRaises(ValueError):
                ledger.observe(source.rows[-1])

    def test_reused_cross_disk_stale_and_broken_predecessor(self):
        tape=Tape();tape.create('node-1',{'value':1});tape.complete('node-1',{'value':2})
        for field,value in (('disk','node-2'),('incarnation',2),('id',1),('predecessor',0)):
            rows=copy.deepcopy(tape.rows)
            row=next(r for r in rows if r['kind']=='disk_transaction' and r['transaction']['id']==2)
            row['transaction'][field]=value
            with self.subTest(field=field),self.assertRaises(ValueError):self.ledger(tape,rows)

    def test_boolean_transaction_alias_is_not_an_identity(self):
        tape=Tape();tape.create('node-1',{'value':1})
        ledger=self.ledger(tape)
        for field in ('id','incarnation','predecessor'):
            token=copy.deepcopy(tape.disks['node-1']['token'])
            token[field]=bool(token[field])
            with self.subTest(field=field),self.assertRaises(ValueError):
                ledger.current({'transaction':token},'node-1')

    def test_reopen_validates_bytes_incarnation_and_continues_chain(self):
        tape=Tape();tape.create('node-1',{'value':1});tape.reopen('node-1');tape.complete('node-1',{'value':2})
        ledger=self.ledger(tape)
        self.assertEqual(ledger.disks['node-1']['incarnation'],2)
        self.assertEqual(V['framed_state'](ledger.disks['node-1']['raw']),{'value':2})
        for field,value in (('predecessor',0),('incarnation',1)):
            rows=copy.deepcopy(tape.rows)
            next(r for r in rows if r['kind']=='disk_reopened')[field]=value
            with self.subTest(field=field),self.assertRaises(ValueError):self.ledger(tape,rows)

    def test_delta_copy_literal_and_parser_bounds(self):
        raw=frame({'first':'a'*100,'last':1})
        copy_all=b'HSRCHW01'+len(raw).to_bytes(4,'little')*2+b'\0'+bytes(4)+len(raw).to_bytes(4,'little')
        self.assertEqual(V['content_delta'](raw,copy_all),raw)
        self.assertEqual(V['content_delta'](b'',literal(b'',raw)),raw)
        faults=[(raw,copy_all[:-1]),(raw,copy_all+b'\xff'),
                (raw,copy_all[:12]+(1048577).to_bytes(4,'little')+copy_all[16:]),
                (raw,copy_all[:17]+(1048576).to_bytes(4,'little')+copy_all[21:]),
                (b'',literal(b'',raw)[:-1]),
                (b'',b'HSRCHW01'+bytes(4)+(16).to_bytes(4,'little')+b'\1'+(2**32-1).to_bytes(4,'little'))]
        for i,(previous,delta) in enumerate(faults):
            with self.subTest(fault=i),self.assertRaises(ValueError):V['content_delta'](previous,delta)

    def test_application_body_membership_and_disk_binding(self):
        for candidate in ('openraft','raft-rs'):
            tape=history(candidate)
            applied=next(r for r in reversed(tape.rows) if r['kind']=='applied')
            applied['transaction']['disk']='node-2-machine' if candidate=='openraft' else 'node-2'
            with self.subTest(candidate=candidate),self.assertRaises(ValueError):self.check_history(tape,candidate)

    def test_poison_requires_reopen_and_failed_attempt_has_no_content(self):
        tape=Tape();tape.create('node-1',{'value':1})
        token=tape.begin('node-1')
        tape.io('disk_error',token,poisoned_until_reload=True)
        ledger=self.ledger(tape)
        self.assertEqual(V['framed_state'](ledger.disks['node-1']['raw']),{'value':1})
        tape.reopen('node-1');tape.complete('node-1',{'value':2})
        ledger=self.ledger(tape)
        self.assertEqual(ledger.disks['node-1']['completed'],3)
        self.assertEqual(ledger.disks['node-1']['token']['predecessor'],1)
        bad=copy.deepcopy(tape.rows)
        bad[:]=[r for r in bad if r['kind']!='disk_reopened']
        with self.assertRaises(ValueError):self.ledger(tape,bad)

    def test_reopen_then_same_identity_body_remains_available(self):
        for candidate in ('openraft','raft-rs'):
            tape=history(candidate)
            name='node-1-machine' if candidate=='openraft' else 'node-1'
            tape.reopen(name)
            token=tape.disks[name]['token']
            tape.emit('applied',node=1,index=1,term=1,log_id=log_id(1),ids=['synthetic'],transaction=token)
            with self.subTest(candidate=candidate):self.check_history(tape,candidate)

    def test_snapshot_compaction_preserves_exact_certified_prefix(self):
        for candidate in ('openraft','raft-rs'):
            tape=history(candidate)
            if candidate=='raft-rs':
                app={'applied':1,'operations':{'synthetic':operation()}}
                snapshot=proto((1,encoded(app)),(2,proto((1,CONF),(2,1),(3,1))))
                token=tape.complete('node-1',raft_state((),1,snapshot=snapshot))
                tape.emit('snapshot_persisted',node=1,index=1,term=1,transaction=token)
            else:
                saved=machine(1)
                saved['snapshot']={'meta':{'last_log_id':log_id(1),
                                   'last_membership':saved['applied']['membership'],'snapshot_id':'synthetic-1'},
                                   'data':list(encoded(saved['applied']))}
                token=tape.complete('node-1-machine',saved)
                tape.emit('snapshot_persisted',node=1,index=1,transaction=token)
                log=open_log();log['purged']=log_id(1)
                tape.complete('node-1-log',log)
            with self.subTest(candidate=candidate):self.check_history(tape,candidate)

    def test_actual_snapshot_corruption_is_quarantined_and_refused(self):
        for candidate in ('openraft','raft-rs'):
            tape=history(candidate)
            if candidate=='raft-rs':
                snapshot=proto((1,encoded({'applied':1,'operations':{'synthetic':operation()}})),
                               (2,proto((1,CONF),(2,1),(3,1))))
                saved=raft_state((),1,snapshot=snapshot)
                name='node-1'
            else:
                saved=machine(1)
                saved['snapshot']={'meta':{'last_log_id':log_id(1),
                                   'last_membership':saved['applied']['membership'],'snapshot_id':'synthetic-1'},
                                   'data':list(encoded(saved['applied']))}
                name='node-1-machine'
            tape.complete(name,saved)
            damaged=copy.deepcopy(saved)
            if candidate=='raft-rs':damaged['snapshot'].pop()
            else:damaged['snapshot']['data'].pop()
            tape.complete(name,damaged)
            tape.emit('truncated_snapshot_refused',node=1)
            with self.subTest(candidate=candidate):self.check_history(tape,candidate)
            tape.emit('applied',node=1,index=1,term=1,log_id=log_id(1),ids=['synthetic'],transaction=tape.disks[name]['token'])
            with self.subTest(reused=candidate),self.assertRaises(ValueError):self.check_history(tape,candidate)

    def test_snapshot_metadata_cannot_relabel_configuration(self):
        tape=history('openraft');saved=machine(1)
        forged=copy.deepcopy(saved['applied']);forged['membership']['membership']['configs']=[[1,2,4]]
        saved['snapshot']={'meta':{'last_log_id':log_id(1),'last_membership':forged['membership'],'snapshot_id':'forged'},
                           'data':list(encoded(forged))}
        tape.complete('node-1-machine',saved)
        with self.assertRaises(ValueError):self.check_history(tape,'openraft')

    def test_applied_membership_nodes_must_match_actual_entry(self):
        tape=history('openraft');saved=machine(1)
        saved['applied']['membership']['membership']['nodes']['4']=None
        tape.complete('node-1-machine',saved)
        with self.assertRaises(ValueError):self.check_history(tape,'openraft')

    def test_log_hole_or_truncated_applied_prefix_is_refused(self):
        for candidate in ('openraft','raft-rs'):
            for hole in (False,True):
                tape=history(candidate)
                if candidate=='raft-rs':
                    tape.complete('node-1',raft_state([raft_entry(2)] if hole else [],1))
                else:
                    tape.complete('node-1-log',open_log([open_entry(2)] if hole else [open_entry(0)]))
                with self.subTest(candidate=candidate,hole=hole),self.assertRaises(ValueError):self.check_history(tape,candidate)

    def test_truncate_uncommitted_suffix_preserves_applied_identity(self):
        for candidate in ('openraft','raft-rs'):
            tape=history(candidate)
            if candidate=='raft-rs':
                suffix=raft_entry(2,operation('uncommitted'))
                tape.complete('node-1',raft_state([raft_entry(),suffix],1))
                ready(tape,1,[suffix]);tape.emit('advance_append',node=1,light_committed_entries=[])
                tape.wire(candidate,[suffix]);tape.complete('node-1',raft_state([raft_entry()],1))
            else:
                suffix=open_entry(2,operation('uncommitted'))
                tape.complete('node-1-log',open_log([open_entry(0),open_entry(1),suffix]));append(tape,1,[suffix])
                tape.wire(candidate,[suffix]);tape.complete('node-1-log',open_log([open_entry(0),open_entry(1)]))
            with self.subTest(candidate=candidate):self.check_history(tape,candidate)


class BundleContentBarrierControls(unittest.TestCase):
    """Synthetic ordering controls, not observed physical directory durability."""
    def fixture(self, area):
        B = runpy.run_path(str(Path(__file__).with_name('native_bundle.py')))
        scratch = area/'scratch'
        scratch.mkdir()
        reports = []
        for i in range(40):
            root = scratch/f'hsearch-durability-raft-rs-1-1-{i}'
            (root/'artifacts').mkdir(parents=True)
            (root/'artifacts/events-000.ndjson').write_bytes(b'{}\n')
            reports.append({'directory':str(root)})
        sample = {'sample_schema':1, 'candidate':'raft-rs', 'profile':'dev', 'elapsed_us':1,
            'workloads':[{'raw':row} for row in reports[:18]], 'failure':reports[18],
            'scales':[{'groups':1, 'raw_groups':reports[19:20]},
                      {'groups':4, 'raw_groups':reports[20:24]}, {'groups':16, 'raw_groups':reports[24:40]}]}
        binding = {key:'a'*64 for key in ('source_digest','lock_sha256','fixture_sha256','envelope_sha256','protoc_sha256')}
        binding.update(source_head='b'*40, image='sha256:'+'c'*64, target='x86_64-unknown-linux-gnu',
                       profile='dev', candidate='raft-rs', repetition=0,
                       execution='diagnostic-cargo-test', nextest_run_id=None)
        destination = area/'bundle'
        receipt = B['collect'](sample, scratch, destination, binding)
        return B, destination, receipt, binding

    def test_every_file_precedes_directory_and_no_journal_or_disposal_claim(self):
        import tempfile
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as temp:
            B, root, receipt, binding = self.fixture(Path(temp))
            calls = []
            with patch.dict(B['seal_completed_bundle'].__globals__,
                            _sync_file=lambda path,_identity:calls.append(('file',path.name)),
                            _sync_directory=lambda path,_identity:calls.append(('directory',path.name))):
                result = B['seal_completed_bundle'](root, receipt, binding)
            self.assertEqual(calls, [('file',item['file']) for item in receipt['files']]+[('directory',root.name)])
            self.assertEqual(result['task_journal'], 'not-committed')
            self.assertFalse(result['disposal_authorized'])
            self.assertEqual(result['binding']['execution'], 'diagnostic-cargo-test')
            self.assertEqual(B['load'](root,receipt,binding)[0]['binding'], binding)

    def test_corrupt_or_orphan_content_refuses_before_sync(self):
        import tempfile
        from unittest.mock import patch
        for orphan in (False, True):
            with tempfile.TemporaryDirectory() as temp:
                B, root, receipt, binding = self.fixture(Path(temp))
                if orphan:(root/'orphan.bin').write_bytes(b'unknown')
                else:(root/'payload-000.bin').write_bytes(b'x'*receipt['files'][1]['bytes'])
                calls = []
                with patch.dict(B['seal_completed_bundle'].__globals__, _sync_file=lambda path,_identity:calls.append(path)):
                    with self.assertRaises(ValueError):B['seal_completed_bundle'](root, receipt, binding)
                self.assertEqual(calls, [])
                self.assertTrue(root.exists())

    def test_failed_or_mutated_sync_cannot_return_a_barrier_or_dispose_files(self):
        import tempfile
        from unittest.mock import patch
        for mutate in (False, True, "identical-replacement"):
            with tempfile.TemporaryDirectory() as temp:
                B, root, receipt, binding = self.fixture(Path(temp))
                def sync_directory(_path, _identity):
                    if mutate == "identical-replacement":
                        import os
                        replacement = root/'replacement.bin'
                        replacement.write_bytes((root/'payload-000.bin').read_bytes())
                        os.replace(replacement, root/'payload-000.bin')
                    elif mutate:(root/'payload-000.bin').write_bytes(b'x'*receipt['files'][1]['bytes'])
                    else:raise OSError('synthetic sync failure')
                with patch.dict(B['seal_completed_bundle'].__globals__, _sync_file=lambda path,_identity:None,
                                _sync_directory=sync_directory):
                    with self.assertRaises((OSError,ValueError)):B['seal_completed_bundle'](root, receipt, binding)
                self.assertEqual({path.name for path in root.iterdir()}, {item['file'] for item in receipt['files']})


if __name__=='__main__':
    unittest.main()
