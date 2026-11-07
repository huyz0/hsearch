"""Real I/O on fabricated closed archives only. No actual retained assets/backend."""
import copy
from contextlib import contextmanager
import hashlib
import io
import json
import os
from pathlib import Path
import runpy
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

from tools.forensic_retention_v1 import protocol as F
from tools.forensic_retention_v1 import _pack as P
from tools.forensic_retention_v1._bytes import Refused
from tools.forensic_retention_v1.storage import Archive, Metadata, DurableMeter, CheckedWriter, audit, identity, decode
from tools.forensic_retention_v1.controller import Controller, ForensicArchive, order
from tools.forensic_retention_v1.lifecycle import TaskLifecycle, ClosedJob, authority, OWNED
from tools.forensic_retention_v1.backend import DockerBackend, canonical_resource_check

ROOT = Path(__file__).resolve().parents[2]


class SourcePort:
    """Test port only. Production TaskLifecycle is exercised independently below."""
    claim = 'm01-17-synthetic'
    owner = F.OWNER
    def __init__(self, binding):
        self.binding = binding
        self.phase = 'allocated'
    def source(self, _image):
        return copy.deepcopy(self.binding)
    def begin(self, manifest, _root):
        if self.phase != 'allocated':
            raise Refused('busy')
        self.phase = 'active'
    def check(self, manifest):
        if self.phase != 'active' or self.binding != manifest['source_binding']:
            raise Refused('source')
    def finish(self, success):
        self.phase = 'complete' if success else 'interrupted'
    def recover(self, manifest, _root):
        if self.phase != 'interrupted' or self.binding != manifest['source_binding']:
            raise Refused('recovery')
        self.phase = 'active'


class ClosedPort:
    def check(self):
        return 'c'*64


class AuditPort:
    """Read the actual pack on the host; no synthetic success receipt."""
    def verify(self, archive, manifest, pin, meter, metadata):
        if any(r['kind'] == 'journal' and r['value'].get('phase') == 'test-worker' for r in metadata.rows()):
            raise Refused('redispatch')
        metadata.append('journal', dict(phase='test-worker'))
        return audit(archive, manifest, pin, meter)


# Incoming independent test-oracle definitions, exact body provenance in PROVENANCE.json.
META=('kind','mode','uid','gid','mtime_ns','windows_attributes','nlink','bytes')
import stat
def snapshot(root):
    root=Path(root).resolve();rows={};pending=list(root.iterdir())
    while pending:
        p=pending.pop();a=p.lstat();assert not p.is_symlink() and not getattr(a,'st_file_attributes',0)&1024
        name=p.relative_to(root).as_posix();assert len(rows)<1024
        directory=stat.S_ISDIR(a.st_mode);assert directory or stat.S_ISREG(a.st_mode) and a.st_nlink==1
        meta=dict(kind='directory' if directory else 'file',mode=stat.S_IMODE(a.st_mode),uid=a.st_uid,gid=a.st_gid,mtime_ns=a.st_mtime_ns,windows_attributes=getattr(a,'st_file_attributes',0),nlink=a.st_nlink,bytes=0 if directory else a.st_size)
        body=None if directory else p.read_bytes();assert directory or len(body)==a.st_size
        rows[name]=dict(metadata=meta,body=body,sha256=None if directory else hashlib.sha256(body).hexdigest())
        if directory:pending.extend(p.iterdir())
    return rows


def assert_reachable(expected,metadata,read_range):
    """Expected bytes/metadata frozen BEFORE subject; read_range is opaque public audit view."""
    for path,row in expected.items():
        got=metadata(path);assert set(got)==set(META),(path,'metadata-fields');assert got==row['metadata'],(path,'metadata-value')
        if row['body'] is None:continue
        chunks=[]
        for offset in range(0,len(row['body']),65536):
            n=min(65536,len(row['body'])-offset);raw=read_range(path,offset,n);assert type(raw)is bytes and len(raw)==n,(path,'range-length');chunks.append(raw)
        joined=b''.join(chunks);assert joined==row['body'],(path,'original-bytes');assert hashlib.sha256(joined).hexdigest()==row['sha256']


class Files(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='owned-forensic-synthetic-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.archive_root = self.root/'archive'
        self.archive_root.mkdir()
        self.meta_root = self.root/'metadata'
        self.meta_root.mkdir()
        # 15 directories and 69 files. Bytes have no relationship to FFF data.
        host = self.archive_root/F.PAIR[0]
        host.mkdir()
        (host/'controller').mkdir()
        (host/'controller/empty-hooks').mkdir()
        export = self.archive_root/F.PAIR[1]
        export.mkdir()
        for i in range(11):
            (export/('case%02d' % i)).mkdir()
        paths = [host/'controller'/n for n in ['execute.py','agent_fixture_worker.py','config.json','request.json']]
        paths += [host/n for n in ['preparation.json','state.json','result.json','result-synthetic.json','failure-synthetic.json']]
        paths += [export/('case%02d' % (i%11))/('item%03d.bin' % i) for i in range(60)]
        q, rem = divmod(224852, 69)
        for i, p in enumerate(paths):
            p.write_bytes(bytes([65+i%26])*(q+(i<rem)))
        self.archive = Archive(self.archive_root)
        # Windows positive tests explicitly inject the durability port. Real
        # fsync behavior is exercised separately, with no native support claim.
        self.sync = (lambda _:None) if os.name == 'nt' else __import__('tools.forensic_retention_v1._sync',fromlist=['sync_directory']).sync_directory
        self.metadata = Metadata(self.meta_root, sync=self.sync)
        self.binding = dict(head='a'*40,tree='b'*40,fingerprint='c'*64,driver_fingerprint='d'*64,image_id='sha256:'+'e'*64,clean=True)
        self.lifecycle = SourcePort(self.binding)
        self.expected = snapshot(self.archive_root)
        self.controller = Controller(self.archive, self.metadata, self.lifecycle, ClosedPort(), AuditPort(), sync=self.sync)
        prep = dict(kind='root-readonly-preparation-v1',task='M01.17',claim=self.lifecycle.claim,
                    owner_session=self.lifecycle.owner,source_binding=self.binding,archive_identity=self.archive.identity)
        self.gate = dict(gate='pass',task_id='M01.17',head=self.binding['head'],tree=self.binding['tree'],packet_sha256='f'*64)
        self.manifest = self.controller.prepare(self.binding, prep, self.gate)
        self.auth = F.authorization(self.manifest, 'Authorize forensic retention of ONLY the 84 FFF originals in '+F.PAIR[0]+' and '+F.PAIR[1]+'; one pack, one probe and one read-only worker; no retries or qualification approval.')
        now = time.time_ns()//1000000
        self.admission = F.admission(self.manifest,self.auth,started_ms=now,deadline_ms=now+1200000)
        self.gate = dict(gate='pass',task_id='M01.17',head=self.binding['head'],tree=self.binding['tree'],packet_sha256='f'*64)
        self.disposition = dict(kind='root-exact84-retention-v1',task='M01.17',claim=self.lifecycle.claim,owner_session=self.lifecycle.owner,
             manifest_sha256=F.digest(self.manifest),source_binding=self.binding,review_packet_sha256='f'*64,human_message_sha256='1'*64,
             archive_identity=self.archive.identity,metadata_root=str(self.meta_root),image_id=self.binding['image_id'],limits=copy.deepcopy(F.LIMITS))

    def run_operation(self):
        return self.controller.run(self.manifest,self.auth,self.admission,self.disposition,self.gate)

    def fresh_meter(self):
        return F.Meter(self.admission)

    def count_originals(self):
        return sum(os.path.lexists(self.archive_root/r['path']) for r in self.manifest['closure']['entries'])

    def test_actual_pack_map_audit_unlink_and_opaque_range(self):
        result = self.run_operation()
        self.assertEqual(result['originals_retired'],84)
        self.assertFalse(result['qualification'])
        self.assertEqual(self.count_originals(),0)
        retained = ForensicArchive(self.archive,self.metadata,self.fresh_meter(),disposition=self.disposition,review_gate=self.gate,claim=self.lifecycle.claim,owner=self.lifecycle.owner)
        self.assertEqual(retained.totals(),dict(entries=84,bytes=224852))
        with retained.open() as pack:
            for row in self.manifest['closure']['entries']:
                if row['kind']=='file':
                    member=pack.member(row['path'],row['sha256'])
                    self.assertEqual(hashlib.sha256(member.read_exact_at(0,member.size)).hexdigest(),row['sha256'])
        with self.assertRaises(ValueError): retained.semantic_reader()
        with self.assertRaises(ValueError): self.run_operation()
        kinds=[r['kind'] for r in self.metadata.rows()]
        self.assertLess(kinds.index('mapping'),kinds.index('audit'))

    def test_every_pre_unlink_phase_retains_all_originals(self):
        # Every phase runs in a separate test instance to avoid reusing a pack.
        self.assertEqual(self.count_originals(),84)
        def fail(phase):
            if phase=='after-audit': raise OSError('injected')
        self.controller.fault=fail
        with self.assertRaises(OSError): self.run_operation()
        self.assertEqual(self.count_originals(),84)
        self.assertEqual(self.lifecycle.phase,'interrupted')
        self.assertIsNotNone(self.metadata.one('mapping'))

    def test_unlink_before_ack_recovery_conserves_every_member(self):
        hits=0
        def fail(phase):
            nonlocal hits
            if phase=='after-unlink':
                hits+=1
                if hits==11: raise OSError('interruption')
        self.controller.fault=fail
        with self.assertRaises(OSError): self.run_operation()
        self.assertEqual(self.count_originals(),73)
        retained=ForensicArchive(self.archive,self.metadata,self.fresh_meter(),disposition=self.disposition,review_gate=self.gate,claim=self.lifecycle.claim,owner=self.lifecycle.owner)
        self.assertEqual(retained.totals(),dict(entries=84,bytes=224852))
        self.controller.fault=lambda _:None
        result=self.controller.recover(self.disposition,self.gate)
        self.assertEqual(result['originals_retired'],84)
        self.assertEqual(self.count_originals(),0)

    def test_metadata_actual_partial_write_is_charged_and_never_overwritten(self):
        self.metadata.append('journal',dict(phase='control'))
        partial=self.meta_root/'001.json'
        partial.write_bytes(b'{')
        with self.assertRaises(ValueError): self.metadata.rows()
        with self.assertRaises(ValueError): self.metadata.append('journal',dict(phase='retry'))
        self.assertEqual(partial.read_bytes(),b'{')

    def test_actual_fsync_failure_preserves_exclusive_pack_and_originals(self):
        original=os.fsync
        def fail(fd):
            if os.fstat(fd).st_size>224852: raise OSError('fsync')
            return original(fd)
        with patch('os.fsync',side_effect=fail),self.assertRaises(OSError): self.run_operation()
        self.assertEqual(self.count_originals(),84)
        self.assertTrue((self.archive_root/self.manifest['pack_name']).exists())
        with self.assertRaises(ValueError): self.metadata.one('mapping')

    def test_corrupt_pack_refuses_before_retirement(self):
        def corrupt(phase):
            if phase=='before-worker':
                with (self.archive_root/self.manifest['pack_name']).open('r+b') as stream:
                    stream.seek(1024); stream.write(b'BAD')
        self.controller.fault=corrupt
        with self.assertRaises(ValueError): self.run_operation()
        self.assertEqual(self.count_originals(),84)

    def test_late_authority_and_source_errors_create_nothing(self):
        for field,value in [('claim','m01-08'),('manifest_sha256','0'*64),('archive_identity',[1,2,3]),('image_id','sha256:'+'0'*64)]:
            bad=copy.deepcopy(self.disposition);bad[field]=value
            with self.subTest(field=field),self.assertRaises(ValueError):
                self.controller.run(self.manifest,self.auth,self.admission,bad,self.gate)
        self.assertEqual(self.metadata.rows(),[])
        self.assertEqual(self.count_originals(),84)
        with self.assertRaises(ValueError): F.authorization(self.manifest,'I approve')

    def test_fixed_scope_failure_history_and_strict_caps(self):
        for defect in ['pair','node','file','bytes','success','proof','raw','deadline','case']:
            m=copy.deepcopy(self.manifest)
            if defect=='pair':m['closure']['groups'][0]='new15'
            if defect=='node':m['closure']['entries'].pop()
            if defect=='file':m['closure']['entries'][0]['kind']='symlink'
            if defect=='bytes':next(r for r in m['closure']['entries'] if r['kind']=='file')['bytes']+=1
            if defect=='success':m['closure']['original_cli_exit']=0
            if defect=='proof':m['closure']['original_host_proof']=True
            if defect=='raw':m['limits']['raw_bytes']=1048576
            if defect=='deadline':m['limits']['duration_ms']+=1
            if defect=='case':m['closure']['entries'][1]['path']=m['closure']['entries'][0]['path'].upper()
            with self.subTest(defect=defect),self.assertRaises(ValueError): F.validate(m)

    def test_audit_pool_and_deadline_sticky(self):
        meter=self.fresh_meter();a=F.audit_for(meter);a.charge(1,rescan=True)
        self.assertEqual(meter.used,1)
        with self.assertRaises(ValueError): meter.charge(268435456)
        with self.assertRaises(ValueError): a.charge(0)
        with self.assertRaises(ValueError): F.Meter(dict(started_ms=1,deadline_ms=2))
        meter=self.fresh_meter()
        for _ in range(32):meter.charge(0,rescan=True)
        with self.assertRaises(ValueError):meter.charge(0,rescan=True)

    def test_json_depth_duplicate_noncanonical_and_bool_bounds(self):
        for raw in [b'{"x":1,"x":2}',b'['*25+b'0'+b']'*25,b'{ "x": 1 }',b'NaN',b'{} '*400000]:
            with self.subTest(raw=raw[:20]),self.assertRaises(ValueError):decode(raw)
        with self.assertRaises(ValueError):F.integer(True,0,1)

    def test_metadata_orphan_alias_and_bound(self):
        (self.meta_root/'unknown').write_bytes(b'opaque')
        with self.assertRaises(ValueError):self.metadata.rows()
        self.assertEqual(self.count_originals(),84)

    def test_independent_original_byte_and_all_metadata_conservation(self):
        self.run_operation()
        retained=ForensicArchive(self.archive,self.metadata,self.fresh_meter(),disposition=self.disposition,review_gate=self.gate,claim=self.lifecycle.claim,owner=self.lifecycle.owner)
        rows={r['path']:r for r in self.manifest['closure']['entries']}
        with retained.open() as pack:
            assert_reachable(self.expected,
                lambda path:{k:rows[path][k] for k in META},
                lambda path,offset,length:pack.member(path,rows[path]['sha256']).read_exact_at(offset,length))

    def test_real_file_fsync_observed_before_any_unlink(self):
        observed=[]
        real_fsync=os.fsync
        real_unlink=Path.unlink
        def fsync(fd):
            result=real_fsync(fd)
            target=os.fstat(fd)
            for path in self.meta_root.iterdir():
                st=path.stat()
                if (st.st_dev,st.st_ino)==(target.st_dev,target.st_ino):
                    try:
                        row=decode(path.read_bytes())
                        if row['kind'] in {'mapping','audit'}:observed.append(row['kind'])
                    except ValueError:pass
            return result
        def unlink(path,*args,**kwargs):
            if path.is_relative_to(self.archive_root):
                self.assertIn('mapping',observed)
                self.assertIn('audit',observed)
            return real_unlink(path,*args,**kwargs)
        with patch('os.fsync',side_effect=fsync),patch.object(Path,'unlink',unlink):
            self.run_operation()
        self.assertLess(observed.index('mapping'),observed.index('audit'))

    def test_durable_io_debits_survive_missing_ack_and_no_budget_reset(self):
        meter=DurableMeter(self.admission,self.metadata)
        meter.charge(1,rescan=True)
        after=DurableMeter(self.admission,self.metadata)
        self.assertEqual(after.used,8388608)
        self.assertEqual(after.rescans,1)
        self.assertEqual(after.deadline,meter.deadline)
        self.metadata.append('journal',dict(phase='io-reservation',bytes=268435456,rescans=32))
        exhausted=DurableMeter(self.admission,self.metadata)
        with self.assertRaises(ValueError):exhausted.charge(1)

    def test_actual_short_pack_write_leaves_only_partial_and_originals(self):
        file=self.root/'short-write'
        meter=self.fresh_meter()
        with file.open('wb') as target:
            class Short:
                def tell(self):return target.tell()
                def write(self,raw):return target.write(raw[:-1])
            with self.assertRaises(ValueError):CheckedWriter(Short(),8192,meter).write(b'abcdef')
        self.assertEqual(file.read_bytes(),b'abcde')
        self.assertEqual(self.count_originals(),84)

    def test_before_and_after_real_map_fsync_failures_never_unlink(self):
        real_fsync=os.fsync
        hit=[]
        def after_fsync(fd):
            result=real_fsync(fd)
            target=os.fstat(fd)
            for path in self.meta_root.iterdir():
                st=path.stat()
                if (st.st_dev,st.st_ino)==(target.st_dev,target.st_ino):
                    row=decode(path.read_bytes())
                    if row['kind']=='mapping':
                        hit.append('actual-map-fsync-returned')
                        raise OSError('lost map fsync acknowledgement')
            return result
        with patch('os.fsync',side_effect=after_fsync),self.assertRaises(OSError):self.run_operation()
        self.assertEqual(hit,['actual-map-fsync-returned'])
        self.assertEqual(self.count_originals(),84)
        self.assertIsNotNone(self.metadata.one('mapping'))
        with self.assertRaises(ValueError):self.metadata.one('audit')

    def test_node_reserve_and_byte_limit_plus_one(self):
        args=(self.manifest['closure'],self.archive.root)
        physical=dict(entries=4053,bytes=224852)
        F.manifest(*args,physical,self.binding,'0'*64)
        for physical in [dict(entries=4096,bytes=224852),dict(entries=84,bytes=67108864)]:
            with self.assertRaises(ValueError):F.manifest(*args,physical,self.binding,'0'*64)
        with self.assertRaises(ValueError):F.audit_cost_bound(2097153,0)
        with self.assertRaises(ValueError):F.audit_cost_bound(2097152,67108864-224852)
        with self.assertRaises(ValueError):F.integer(65537,0,65536)

    def test_changed_file_identity_and_nonprefix_recovery_refuse(self):
        def fail(phase):
            if phase=='after-audit':raise OSError('stop')
        self.controller.fault=fail
        with self.assertRaises(OSError):self.run_operation()
        rows=order(self.manifest)
        (self.archive_root/rows[3]['path']).unlink()
        self.controller.fault=lambda _:None
        with self.assertRaises(ValueError):self.controller.recover(self.disposition,self.gate)
        self.assertEqual(self.count_originals(),83)

    def test_range_overflow_corruption_and_wrong_member_digest_refuse(self):
        self.run_operation()
        retained=ForensicArchive(self.archive,self.metadata,self.fresh_meter(),disposition=self.disposition,review_gate=self.gate,claim=self.lifecycle.claim,owner=self.lifecycle.owner)
        row=next(r for r in self.manifest['closure']['entries'] if r['kind']=='file')
        with retained.open() as pack:
            with self.assertRaises(ValueError):pack.member(row['path'],'0'*64)
            member=pack.member(row['path'],row['sha256'])
            for offset,length in [(-1,1),(0,65537),(member.size,1),(True,1)]:
                with self.subTest(offset=offset,length=length),self.assertRaises(ValueError):member.read_exact_at(offset,length)

    def test_metadata_record_and_journal_cap_plus_one(self):
        for i in range(72):self.metadata.append('journal',dict(phase='cap',i=i))
        with self.assertRaises(ValueError):self.metadata.append('journal',dict(phase='overflow'))
        for i in range(24):self.metadata.append('worker',dict(i=i))
        with self.assertRaises(ValueError):self.metadata.append('worker',dict(i=97))
        self.assertEqual(len(list(self.meta_root.iterdir())),96)

    def test_nonregular_hardlink_and_outside_change_refuse(self):
        row=next(r for r in self.manifest['closure']['entries'] if r['kind']=='file')
        path=self.archive_root/row['path']
        alias=self.root/'alias'
        os.link(path,alias)
        with self.assertRaises(ValueError):self.run_operation()
        self.assertEqual(self.count_originals(),84)

    def stopped_before_worker(self):
        def stop(phase):
            if phase=='before-worker':raise OSError('transport fixture stop')
        self.controller.fault=stop
        with self.assertRaises(OSError):self.run_operation()
        path=self.archive_root/self.manifest['pack_name']
        return hashlib.sha256(path.read_bytes()).hexdigest()

    def transport(self, pack_sha, *, fail_create=False, corrupt_probe=False):
        commands=[]
        ids=[]
        def capture(argv,*,timeout,limit):
            self.assertGreater(timeout,0)
            self.assertLessEqual(limit,8192)
            args=argv[3:];commands.append(args)
            if args[0]=='info':return json.dumps(dict(ID='synthetic-engine',OSType='linux',CgroupVersion='2'))
            if args[0]=='context':return 'unix:///owned/synthetic.sock\n'
            if args[0]=='ps':return ''
            if args[0]=='create':
                self.assertIn('--read-only',args)
                self.assertIn('--network=none',args)
                self.assertIn('--pids-limit=256',args)
                self.assertIn('--memory-swap=4294967296',args)
                self.assertEqual(len([a for a in args if 'type=bind,' in a and a.endswith(',readonly')]),5)
                if fail_create:raise OSError('unknown create acknowledgement')
                cid=str(len(ids)+1)*64;ids.append(cid);return cid+'\n'
            if args[0]=='start':
                if args[-1]==ids[0]:
                    caps=dict(kind='forensic-cgroup-probe-v1',cpu_millis=2000,memory_bytes=4294967296,swap_bytes=0,pids=256,scratch_bytes=2147483648,uid=10001)
                    if corrupt_probe:caps['pids']=257
                    return F.enc(caps).decode()
                return F.enc(audit(self.archive,self.manifest,pack_sha,self.fresh_meter())).decode()
            if args[0]=='inspect':return 'exited 0\n'
            if args[0]=='rm':return args[1]+'\n'
            raise AssertionError('unhandled command')
        checks=[]
        backend=DockerBackend({'SANDBOX':{'bounded_capture':capture}},ROOT/'tools/forensic_retention_v1',context='synthetic',engine_id='synthetic-engine',resource_check=lambda m:checks.append(F.digest(m)))
        return backend,commands,checks

    def test_production_transport_contract_one_probe_one_worker(self):
        pin=self.stopped_before_worker()
        backend,commands,checks=self.transport(pin)
        result=backend.verify(self.archive,self.manifest,pin,self.fresh_meter(),self.metadata)
        self.assertEqual(result,audit(self.archive,self.manifest,pin,self.fresh_meter()))
        self.assertEqual(len([c for c in commands if c[0]=='create']),2)
        self.assertEqual(len([c for c in commands if c[0]=='start']),2)
        self.assertTrue(all('-f' not in c and '--force' not in c for c in commands))
        self.assertLessEqual(backend.raw,8192)
        self.assertEqual(len(checks),4)
        with self.assertRaises(ValueError):backend.verify(self.archive,self.manifest,pin,self.fresh_meter(),self.metadata)
        self.assertEqual(self.count_originals(),84)

    def test_unknown_create_never_redispatches_or_unlinks(self):
        pin=self.stopped_before_worker()
        backend,commands,_=self.transport(pin,fail_create=True)
        with self.assertRaises(OSError):backend.verify(self.archive,self.manifest,pin,self.fresh_meter(),self.metadata)
        with self.assertRaises(ValueError):backend.verify(self.archive,self.manifest,pin,self.fresh_meter(),self.metadata)
        self.assertEqual(len([c for c in commands if c[0]=='create']),1)
        self.assertFalse(any(c[0]=='start' for c in commands))
        self.assertEqual(self.count_originals(),84)

    def test_probe_wrong_kernel_cap_prevents_worker(self):
        pin=self.stopped_before_worker()
        backend,commands,_=self.transport(pin,corrupt_probe=True)
        with self.assertRaises(ValueError):backend.verify(self.archive,self.manifest,pin,self.fresh_meter(),self.metadata)
        self.assertEqual(len([c for c in commands if c[0]=='create']),1)
        self.assertEqual(self.count_originals(),84)

    def test_fixed_worker_reads_real_pack_without_writes(self):
        from tools.forensic_retention_v1 import worker
        from types import SimpleNamespace
        pin=self.stopped_before_worker()
        output=io.BytesIO()
        before={p:p.read_bytes() for p in self.meta_root.iterdir()}
        inputs={k:self.metadata.one(k) for k in ['manifest','authorization','admission']}
        with patch.object(worker,'load_inputs',return_value=inputs),patch.object(worker,'Archive',return_value=self.archive),patch.object(worker.sys,'stdout',SimpleNamespace(buffer=output)):
            code=worker.main(['audit',pin])
        self.assertEqual(code,0)
        self.assertEqual(decode(output.getvalue().strip()),audit(self.archive,self.manifest,pin,self.fresh_meter()))
        self.assertEqual(before,{p:p.read_bytes() for p in self.meta_root.iterdir()})

    def test_worker_loads_only_three_pinned_record_files(self):
        from tools.forensic_retention_v1 import worker
        self.stopped_before_worker()
        inputs=self.root/'inputs';inputs.mkdir()
        for kind in ['manifest','authorization','admission']:
            (inputs/(kind+'.json')).write_bytes(self.metadata.path_for(kind).read_bytes())
        with patch.object(worker,'Path',return_value=inputs):
            loaded=worker.load_inputs()
            self.assertEqual(loaded['manifest'],self.manifest)
            self.assertEqual(loaded['authorization'],self.auth)
        self.assertEqual(self.count_originals(),84)

    def test_guest_capability_cannot_drive_host_retirement(self):
        guest=Archive(self.archive_root,host_private=False)
        with self.assertRaises(ValueError):Controller(guest,self.metadata,self.lifecycle,ClosedPort(),AuditPort(),sync=self.sync)
        guest.check()

    def test_source_change_before_map_refuses_before_publication(self):
        def change(phase):
            if phase=='before-map':self.lifecycle.binding['fingerprint']='9'*64
        self.controller.fault=change
        with self.assertRaises(ValueError):self.run_operation()
        self.assertEqual(self.count_originals(),84)
        with self.assertRaises(ValueError):self.metadata.one('mapping')

    def test_actual_short_map_write_is_retained_as_an_orphan(self):
        real_open=Path.open
        hits=[]
        class Short:
            def __init__(inner,target):inner.target=target
            def __enter__(inner):return inner
            def __exit__(inner,*args):inner.target.close()
            def __getattr__(inner,name):return getattr(inner.target,name)
            def write(inner,raw):
                if b'"kind":"mapping"' in raw:
                    hits.append(inner.target.name)
                    return inner.target.write(raw[:-1])
                return inner.target.write(raw)
        def opened(path,mode='r',*args,**kwargs):
            target=real_open(path,mode,*args,**kwargs)
            return Short(target) if path.parent==self.meta_root and mode=='xb' else target
        with patch.object(Path,'open',opened),self.assertRaises(ValueError):self.run_operation()
        self.assertEqual(len(hits),1)
        self.assertTrue(Path(hits[0]).read_bytes())
        with self.assertRaises(ValueError):self.metadata.rows()
        self.assertEqual(self.count_originals(),84)

    def test_complete_preparation_cli_private_public_boundary(self):
        from tools.forensic_retention_v1 import __main__ as cli
        from types import SimpleNamespace
        artifacts=self.root/'artifacts';artifacts.mkdir()
        self.lifecycle.source_root=self.root/'source'
        config=self.root/'prepare-config.json'
        config.write_bytes(F.enc(dict(artifact_root=str(artifacts),state_root=str(self.root/'state'),resource_root=str(self.root/'resource'))))
        evidence=self.root/'prepare-evidence.json'
        prep=dict(kind='root-readonly-preparation-v1',task='M01.17',claim=self.lifecycle.claim,owner_session=self.lifecycle.owner,
                  source_binding=self.binding,archive_identity=self.archive.identity)
        evidence.write_bytes(F.enc(dict(source_binding=self.binding,preparation=prep,review_gate=self.gate)))
        out=io.BytesIO()
        with patch.object(cli,'load',return_value=self.controller),patch.object(cli,'sync_directory',self.sync),patch.object(cli.sys,'stdout',SimpleNamespace(buffer=out)):
            self.assertEqual(cli.main(['prepare','--config',str(config),'--evidence',str(evidence)]),0)
        summary=decode(out.getvalue().strip())
        self.assertEqual(summary['manifest_sha256'],F.digest(self.manifest))
        self.assertNotIn(F.PAIR[0].encode(),out.getvalue())
        self.assertEqual(self.metadata.rows(),[])
        self.assertEqual(self.count_originals(),84)

    def test_canonical_resource_check_uses_existing_readonly_snapshot(self):
        from types import SimpleNamespace
        path=self.root/'registry.json'
        row=dict(kind=F.OP,task_claim=self.lifecycle.claim,owner_session=self.lifecycle.owner,phase='ready',image_id=self.binding['image_id'],manifest_sha256=F.digest(self.manifest),limits=F.LIMITS)
        raw=F.enc(dict(schema_version=1,leases={'synthetic':row}));path.write_bytes(raw)
        check=canonical_resource_check(SimpleNamespace(path=path),task_claim=self.lifecycle.claim,owner=self.lifecycle.owner,namespace='synthetic',image_id=self.binding['image_id'])
        check(self.manifest)
        self.assertEqual(path.read_bytes(),raw)
        row['owner_session']='foreign'
        path.write_bytes(F.enc(dict(schema_version=1,leases={'synthetic':row})))
        with self.assertRaises(ValueError):check(self.manifest)

    def test_cli_unknown_evidence_stays_payload_free(self):
        from tools.forensic_retention_v1 import __main__ as cli
        config=self.root/'config.json';config.write_bytes(b'{}')
        evidence=self.root/'input.json';evidence.write_bytes(b'{}')
        error=io.StringIO()
        with patch.object(cli.sys,'stderr',error):code=cli.main(['audit','--config',str(config),'--evidence',str(evidence)])
        self.assertEqual(code,1)
        self.assertEqual(error.getvalue(),'forensic-operation-refused; preserve existing evidence\n')

    def closed_record(self):
        record=dict(kind='failed-forensic-closed-job-v1',claim=F.CLAIM,owner_session=F.OWNER,groups=list(F.PAIR),phase='closed',
                    controller=dict(pid=2147483647,identity='dead-original-birth'),container_ids=['3'*64],
                    source_head='fffbe1142717f45d538b2979021719db7f09d302',source_tree='86c6636f934d1648747a5d9d7849c2b776f48740',
                    cli_exit=1,guest_tests=601,host_proof=False,cause='unconfirmed')
        return record

    def test_closed_record_ids_leases_and_birth_are_independently_checked(self):
        from types import SimpleNamespace
        path=self.root/'closed.json';registry=self.root/'resources.json'
        record=self.closed_record();path.write_bytes(F.enc(record));registry.write_bytes(F.enc(dict(schema_version=1,leases={})))
        ids=[]
        closed=ClosedJob({'handle_live':lambda _:False},path,F.digest(record),SimpleNamespace(path=registry),SimpleNamespace(absent=lambda x:ids.extend(x)))
        before=registry.read_bytes()
        self.assertEqual(closed.check(),F.digest(record))
        self.assertEqual(ids,['3'*64])
        self.assertEqual(registry.read_bytes(),before)
        closed.api['handle_live']=lambda _:True
        with self.assertRaises(ValueError):closed.check()
        closed.api['handle_live']=lambda _:False
        registry.write_bytes(F.enc(dict(schema_version=1,leases={'unknown':dict(task_claim=F.CLAIM,phase='unknown')})))
        with self.assertRaises(ValueError):closed.check()

    def test_actual_live_birth_and_changed_closure_cannot_authorize_reads(self):
        from types import SimpleNamespace
        api=runpy.run_path(str(ROOT/'tools/task.py'))
        path=self.root/'closed-live.json';registry=self.root/'empty-registry.json'
        registry.write_bytes(F.enc(dict(schema_version=1,leases={})))
        record=self.closed_record();record['controller']=dict(pid=os.getpid(),identity=api['SANDBOX']['owner_identity']())
        path.write_bytes(F.enc(record))
        closed=ClosedJob(api,path,F.digest(record),SimpleNamespace(path=registry),SimpleNamespace(absent=lambda _:None))
        with self.assertRaises(ValueError):closed.check()
        record['cli_exit']=0;path.write_bytes(F.enc(record))
        with self.assertRaises(ValueError):closed.check()

    def test_deadline_refusal_remains_sticky_if_wall_clock_reverses(self):
        clock=[self.admission['started_ms']]
        meter=F.Meter(self.admission,clock=lambda:clock[0])
        clock[0]=self.admission['deadline_ms']
        with self.assertRaises(ValueError):meter.check()
        clock[0]=self.admission['started_ms']
        with self.assertRaises(ValueError):meter.check()

    def test_cli_run_and_audit_real_fixture_emit_only_bounded_summaries(self):
        from tools.forensic_retention_v1 import __main__ as cli
        from types import SimpleNamespace
        config=self.root/'cli-config.json';config.write_bytes(b'{}')
        evidence=self.root/'cli-evidence.json'
        evidence.write_bytes(F.enc(dict(manifest=self.manifest,authorization=self.auth,admission=self.admission,
                                       disposition=self.disposition,review_gate=self.gate)))
        out=io.BytesIO()
        with patch.object(cli,'load',return_value=self.controller),patch.object(cli.sys,'stdout',SimpleNamespace(buffer=out)):
            self.assertEqual(cli.main(['run','--config',str(config),'--evidence',str(evidence)]),0)
        self.assertLessEqual(len(out.getvalue()),16384)
        self.assertEqual(decode(out.getvalue().strip())['originals_retired'],84)
        evidence.write_bytes(F.enc(dict(disposition=self.disposition,review_gate=self.gate)))
        out=io.BytesIO()
        with patch.object(cli,'load',return_value=self.controller),patch.object(cli.sys,'stdout',SimpleNamespace(buffer=out)):
            self.assertEqual(cli.main(['audit','--config',str(config),'--evidence',str(evidence)]),0)
        self.assertEqual(decode(out.getvalue().strip())['logical'],dict(entries=84,bytes=224852))

    def test_private_manifest_exclusive_slots_and_bytes(self):
        from tools.forensic_retention_v1 import __main__ as cli
        root=self.root/'private-artifacts';root.mkdir()
        with patch.object(cli,'sync_directory',self.sync):
            first=cli.publish_manifest(self.manifest,root)
            with self.assertRaises(FileExistsError):cli.publish_manifest(self.manifest,root)
            other=dict(self.manifest);other['test_only_extra']=1
            cli.publish_manifest(other,root)
            with self.assertRaises(ValueError):cli.publish_manifest(other,root)
        self.assertEqual(first['manifest_sha256'],F.digest(self.manifest))
        self.assertEqual(self.count_originals(),84)

    def test_worker_probe_reads_actual_bounded_private_cgroup_fixture(self):
        from tools.forensic_retention_v1 import worker
        from types import SimpleNamespace
        cgroup=self.root/'cgroup';cgroup.mkdir()
        for name,value in {'cpu.max':'200000 100000','memory.max':'4294967296','memory.swap.max':'0','pids.max':'256'}.items():
            (cgroup/name).write_text(value)
        with patch.object(worker,'Path',return_value=cgroup),patch.object(worker.os,'statvfs',return_value=SimpleNamespace(f_blocks=524288,f_frsize=4096),create=True),patch.object(worker.os,'getuid',return_value=10001,create=True):
            result=worker.probe()
            self.assertEqual(result['scratch_bytes'],2147483648)
            self.assertEqual(result['cpu_millis'],2000)
            (cgroup/'cpu.max').write_bytes(b'1'*129)
            with self.assertRaises(ValueError):worker.probe()


def _phase_control(phase):
    def control(self):
        def fail(name):
            if name==phase:raise OSError('seeded phase interruption')
        self.controller.fault=fail
        with self.assertRaises(OSError):self.run_operation()
        self.assertEqual(self.count_originals(),84)
        self.assertNotEqual(self.lifecycle.phase,'complete')
    return control

for _phase in ['before-exclusive','after-exclusive','member-write','pack-file-sync','pack-directory-sync',
               'before-worker','before-map','after-map','after-audit','before-retire-intent','before-unlink']:
    setattr(Files,'test_phase_'+_phase.replace('-','_'),_phase_control(_phase))


class TaskStoreCompatibility(unittest.TestCase):
    def test_real_default_taskstore_refuses_foreign_forensic_phases(self):
        api=runpy.run_path(str(ROOT/'tools/task.py'))
        with tempfile.TemporaryDirectory(prefix='owned-taskstore-forensic-') as tmp:
            root=Path(tmp);repo=root/'repo';repo.mkdir()
            def git(*args):
                return subprocess.check_output(['git',*args],cwd=repo,stderr=subprocess.PIPE,text=True).strip()
            git('init','-q');git('config','user.name','Synthetic');git('config','user.email','synthetic@localhost')
            git('config','core.autocrlf','false')
            (repo/'docs/plan').mkdir(parents=True)
            (repo/'docs/plan/tasks.json').write_bytes((ROOT/'docs/plan/tasks.json').read_bytes())
            (repo/'tools/forensic_retention_v1').mkdir(parents=True)
            (repo/'tools/forensic_retention_v1/__init__.py').write_text('"""synthetic"""\n')
            git('add','.');git('commit','-qm','synthetic fixture')
            store=api['TaskStore'](root/'state',root=repo)
            packet=dict(schema_version=1,task_id='M01.17',goal='synthetic \u03b1',base_revision=git('rev-parse','HEAD'),owner_session=F.OWNER,
                        owned_paths=OWNED,assumptions=[],acceptance=['synthetic'],negative_cases=['busy refuses'],commands=['check'],
                        output_budget_bytes=16384,expected_evidence=['synthetic'])
            claim=store.claim(packet)
            lifecycle=TaskLifecycle(api,store,claim,F.OWNER)
            binding=lifecycle.source('sha256:'+'e'*64)
            # Bind a real temporary directory. No actual FFF archive path.
            archive=root/'archive';archive.mkdir();metadata=root/'meta';metadata.mkdir()
            manifest=dict(source_binding=binding,payload_plan=dict(archive_root=str(archive)))
            lifecycle.begin(manifest,metadata)
            for phase in ['forensic-active','forensic-interrupted','forensic-complete']:
                with store.edit() as data:
                    data['tasks'][claim]['phase']=phase
                for action in ['run','resume','cleanup']:
                    with self.subTest(phase=phase,action=action),self.assertRaises(api['Refused']):
                        if action=='run':store.run(claim,F.OWNER,'check',root/'resources')
                        else:getattr(store,action)(claim,F.OWNER,root/'resources')
            with store.edit() as data:
                data['tasks'][claim]['phase']='forensic-active'
            lifecycle.finish(False)
            self.assertEqual(store.get(claim,F.OWNER)['phase'],'forensic-interrupted')
            lifecycle.recover(manifest,metadata)
            lifecycle.check(manifest)
            lifecycle.finish(False)
            with self.assertRaises(ValueError):TaskLifecycle(api,store,claim,'foreign-owner')
            for _ in range(4):lifecycle.source('sha256:'+'e'*64)
            with self.assertRaises(ValueError):lifecycle.source('sha256:'+'e'*64)
            # This disposable fixture uses normal clean Git removal, no force.
            subprocess.check_call(['git','worktree','remove',store.get(claim,F.OWNER)['worktree']],cwd=repo,
                                  stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)

    def test_real_coordinator_loader_binds_existing_canonical_state_without_rewrite(self):
        from tools.forensic_retention_v1 import __main__ as cli
        from types import SimpleNamespace
        import shutil
        with tempfile.TemporaryDirectory(prefix='owned-loader-fixture-') as tmp:
            root=Path(tmp);repo=root/'repo';repo.mkdir()
            def git(*args):
                return subprocess.check_output(['git',*args],cwd=repo,stderr=subprocess.PIPE,text=True).strip()
            git('init','-q');git('config','core.autocrlf','false');git('config','user.name','Synthetic');git('config','user.email','synthetic@localhost')
            for name in ['tools/task.py','tools/sandbox.py','harness/task-packet.schema.json','docs/plan/tasks.json']:
                target=repo/name;target.parent.mkdir(parents=True,exist_ok=True);target.write_bytes((ROOT/name).read_bytes())
            shutil.copytree(ROOT/'tools/forensic_retention_v1',repo/'tools/forensic_retention_v1')
            git('add','.');git('commit','-qm','synthetic loader source')
            api=runpy.run_path(str(repo/'tools/task.py'));store=api['TaskStore'](root/'state',root=repo)
            packet=dict(schema_version=1,task_id='M01.17',goal='loader synthetic',base_revision=git('rev-parse','HEAD'),owner_session=F.OWNER,
                        owned_paths=OWNED,assumptions=[],acceptance=['synthetic'],negative_cases=['foreign state refuses'],commands=['check'],
                        output_budget_bytes=16384,expected_evidence=['synthetic'])
            claim=store.claim(packet);row=store.get(claim,F.OWNER);worktree=Path(row['worktree'])
            resources=root/'resources';registry=api['SANDBOX']['Registry'](resources)
            with registry.edit():pass
            with patch.dict(api['SANDBOX'],Sandbox=lambda *a,**k:SimpleNamespace()):store.resources(resources,row)
            archive=root/'archive';archive.mkdir();metadata=root/'metadata';metadata.mkdir();artifacts=root/'artifacts';artifacts.mkdir()
            closed=root/'closed.json';closed.write_bytes(b'{}')
            config=dict(state_root=str(root/'state'),resource_root=str(resources),claim=claim,owner_session=F.OWNER,
                        archive_root=str(archive),metadata_root=str(metadata),closed_record=str(closed),closed_record_sha256=hashlib.sha256(b'{}').hexdigest(),
                        docker_context='synthetic',engine_id='synthetic',resource_namespace='synthetic',image_id='sha256:'+'e'*64,artifact_root=str(artifacts))
            before=(store.path.read_bytes(),store.path.stat().st_mtime_ns,registry.path.read_bytes(),registry.path.stat().st_mtime_ns)
            controller=cli.load(config,source_root=worktree)
            self.assertEqual(controller.lifecycle.claim,claim)
            self.assertEqual(controller.lifecycle.source_root,worktree)
            after=(store.path.read_bytes(),store.path.stat().st_mtime_ns,registry.path.read_bytes(),registry.path.stat().st_mtime_ns)
            self.assertEqual(before,after)
            bad=dict(config);bad['owner_session']='foreign'
            with self.assertRaises(ValueError):cli.load(bad,source_root=worktree)
            subprocess.check_call(['git','worktree','remove',str(worktree)],cwd=repo,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)


if __name__=='__main__':unittest.main()
