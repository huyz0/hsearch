"""Synthetic ports only. These controls grant no native or FFF operation proof."""
from dataclasses import asdict, replace
import ast
import copy
import hashlib
import io
import json
import pathlib
import sys
import tarfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2]))
from tools.opaque_retention_image_v1 import image as I
from tools.retention_context_v1.context import Domain, Original, Receipt, Sample, Session
from tools.retention_context_v1.codec import Refused


def h(raw):
    return hashlib.sha256(raw).hexdigest()


def wire(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()


class Clock:
    """A fixed, synthetic same-domain clock. No host-clock qualification."""
    def __init__(self):
        self.tick = 100

    def sample(self):
        return Sample(self.tick, self.tick, self.tick, Domain('1'*64, '2'*64, '3'*64, '4'*64))


class Store:
    """In-memory acknowledged CAS, not a persistence/antirollback claim."""
    def __init__(self):
        self.raw = None
        self.ack = True

    def load(self):
        return Receipt(self.raw, self.ack)

    def compare_exchange(self, expected, replacement):
        if self.raw != expected:
            return Receipt(self.raw, False)
        self.raw = replacement
        return Receipt(self.raw, self.ack)


def context():
    clock, store = Clock(), Store()
    original = Original.begin('5'*64, '6'*64, '7'*64, clock, 1000000)
    session = Session.publish(original, store, clock)
    return original, session, store, clock


def fixture():
    # Independent synthetic inventory: two roots, thirteen directories,69files.
    roots = ['hsearch-agent-fixture-3686832deb7c', 'sealed-export-qual3686832deb7c']
    rows, data = [], {}

    def entry(path, kind, raw=b''):
        return dict(path=path, kind=kind, mode=0o755 if kind == 'directory' else 0o640,
                    uid=1000, gid=1001, mtime_ns=1700000000123456789,
                    windows_attributes=16 if kind == 'directory' else 32,
                    nlink=2 if kind == 'directory' else 1, bytes=len(raw),
                    sha256=None if kind == 'directory' else h(raw))

    rows.extend(entry(root, 'directory') for root in roots)
    directories = [roots[0]+'/d'+str(i).zfill(2) for i in range(13)]
    rows.extend(entry(p, 'directory') for p in directories)
    sizes = [70000, 0]+[2048]*66+[19684]
    assert len(sizes) == 69 and sum(sizes) == 224852
    for i, size in enumerate(sizes):
        path = (directories[i % 13] if i % 2 == 0 else roots[1])+'/file'+str(i).zfill(2)
        raw = bytes([i+1])*size
        data[path] = raw
        rows.append(entry(path, 'file', raw))
    value = dict(schema='hsearch-opaque-input-v1', version=1, entries=sorted(rows, key=lambda e:e['path']),
                 history=dict(original_cli_exit=1, guest_tests=601, original_host_proof=False,
                    cause='unconfirmed', qualification=False, semantic_export=False,
                    original_source_head='fffbe1142717f45d538b2979021719db7f09d302',
                    original_source_tree='86c6636f934d1648747a5d9d7849c2b776f48740',
                    original_handoff_sha256='1b0d0e6bcd32f9919955714ad10e2fb4b5098bab0cbd60b7500de91ce1d4e0d8',
                    original_cli_log_sha256='6d810a135da56b72f50db55d830cd8d88ed905ef71e573197e8aa7bef1990457'))
    return value, data


class Source:
    def __init__(self, value, data):
        self.rows = {e['path']: I.Entry(**e) for e in value['entries']}
        self.data = dict(data)
        self.alias = False
        self.calls = 0

    def stat(self, name):
        self.calls += 1
        return I.SourceState(self.rows[name], h(b'alias' if self.alias else name.encode()))

    def read_exact_at(self, name, offset, length):
        assert 0 <= length <= 65536
        return self.data[name][offset:offset+length]


class Port:
    """Bounded memory adapter with explicit short/unknown/sync/stamp faults."""
    def __init__(self, raw=b''):
        self.raw = bytearray(raw)
        self.identity = h(b'image')
        self.revision = 0
        self.fault = None
        self.calls = []

    def describe(self):
        return I.Blob(self.identity, h(str(self.revision).encode()), len(self.raw))

    def write_exact_at(self, offset, raw):
        assert offset == len(self.raw) and len(raw) <= 65536
        self.calls.append(('write', offset, len(raw)))
        self.raw.extend(raw)
        self.revision += 1
        if self.fault == 'short':
            return len(raw)-1
        if self.fault == 'unknown':
            raise OSError('synthetic write uncertainty with secret payload')
        if self.fault == 'bool':
            return True
        return len(raw)

    def sync(self):
        self.calls.append(('sync',))
        if self.fault == 'sync':
            return False
        if self.fault == 'sync-error':
            raise OSError('synthetic sync fault')
        return True

    def read_exact_at(self, offset, length):
        assert length <= 65536
        self.calls.append(('read', offset, length))
        raw = bytes(self.raw[offset:offset+length])
        if self.fault == 'read-short' and raw:
            return raw[:-1]
        if self.fault == 'read-long':
            return raw+b'x'
        return raw


class ImageTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.value, cls.data = fixture()
        original, session, _, _ = context()
        raw = wire(cls.value)
        cls.plan = I.plan(raw, h(raw), original, I.Physical(3500, 2000000))
        port = Port()
        view = I.Operation(cls.plan, session).build(Source(cls.value, cls.data), port)
        cls.raw, cls.evidence = bytes(port.raw), view.evidence

    def fresh(self, raw=None):
        original, session, store, clock = context()
        self.assertEqual(original, self.plan.original)
        port = Port(self.raw if raw is None else raw)
        return I.Operation(self.plan, session), port, store, clock

    def parse(self, value=None, physical=None, original=None):
        raw = wire(self.value if value is None else value)
        return I.plan(raw, h(raw), self.plan.original if original is None else original,
                      self.plan.physical if physical is None else physical)

    def test_independent_ustar_byte_and_every_metadata_oracle(self):
        # Python's tarfile parser, original fixture dictionaries and bytes are
        # independent of the production header/index decoder and view.
        with tarfile.open(fileobj=io.BytesIO(self.raw), mode='r:') as archive:
            members = archive.getmembers()
            self.assertEqual(len(members), 85)
            self.assertEqual([m.name.rstrip('/') for m in members[:-1]], [e['path'] for e in self.value['entries']])
            for member, row in zip(members, self.value['entries']):
                self.assertEqual((member.mode, member.uid, member.gid, member.mtime, member.size),
                                 (row['mode'], row['uid'], row['gid'], row['mtime_ns']//1000000000, row['bytes']))
                self.assertEqual(member.isdir(), row['kind'] == 'directory')
                if row['kind'] == 'file':
                    self.assertEqual(archive.extractfile(member).read(), self.data[row['path']])
            index = json.loads(archive.extractfile(members[-1]).read())
            self.assertEqual(index['entries'], self.value['entries'])
            self.assertEqual(index['history'], self.value['history'])
            self.assertEqual(index['original'], asdict(self.plan.original))
            self.assertEqual(index['manifest_sha256'], h(wire(self.value)))
        self.assertEqual(self.evidence.pack_sha256, h(self.raw))
        self.assertEqual(self.evidence.payload_bytes, 224852)
        self.assertTrue(self.evidence.opaque_only)
        self.assertFalse(self.evidence.qualification)
        self.assertLessEqual(len(self.raw), 2097152)

    def test_deterministic_build(self):
        _, session, _, _ = context()
        port = Port()
        view = I.Operation(self.plan, session).build(Source(self.value, self.data), port)
        self.assertEqual(bytes(port.raw), self.raw)
        self.assertEqual(view.evidence, self.evidence)
        self.assertTrue(any(c[0] == 'read' for c in port.calls))
        sync_at = next(i for i,c in enumerate(port.calls) if c[0] == 'sync')
        self.assertTrue(all(c[0] != 'read' for c in port.calls[:sync_at]))

    def test_opaque_range_crosses_chunk_boundary_and_empty_eof(self):
        op, port, _, _ = self.fresh()
        view = op.open(port, h(self.raw))
        name = next(n for n,b in self.data.items() if len(b) == 70000)
        self.assertEqual(view.read(name, h(self.data[name]), 65000, 5000), self.data[name][65000:70000])
        self.assertEqual(view.read(name, h(self.data[name]), 70000, 0), b'')
        empty = next(n for n,b in self.data.items() if not b)
        self.assertEqual(view.read(empty, h(b''), 0, 0), b'')
        self.assertEqual(tuple(asdict(e) for e in view.entries()), tuple(self.value['entries']))
        self.assertFalse(hasattr(view, 'semantic_reader'))
        self.assertFalse(hasattr(view, 'export'))

    def test_manifest_shape_types_and_history_are_not_self_approved(self):
        cases=[]
        for field,value in [('version',True),('version',2),('schema','unknown')]:
            v=copy.deepcopy(self.value);v[field]=value;cases.append(v)
        v=copy.deepcopy(self.value);v['extra']=1;cases.append(v)
        for field,value in [('qualification',True),('original_cli_exit',0),('original_host_proof',True),('guest_tests',601.0)]:
            v=copy.deepcopy(self.value);v['history'][field]=value;cases.append(v)
        for value in cases:
            with self.subTest(value=str(value)[:50]), self.assertRaises(Refused):self.parse(value)

    def test_inventory_count_payload_scope_order_parent_and_alias(self):
        cases=[]
        for edit in ['count','payload','scope','order','parent','alias','root','file-parent']:
            v=copy.deepcopy(self.value);rows=v['entries']
            if edit=='count':rows.pop()
            elif edit=='payload':next(e for e in rows if e['kind']=='file')['bytes']+=1
            elif edit=='scope':rows[0]['path']='outside'
            elif edit=='order':rows.reverse()
            elif edit=='parent':next(e for e in rows if e['kind']=='file')['path']=I.PAIR[0]+'/absent/file'
            elif edit=='alias':rows[2]['path']=rows[1]['path'].rsplit('/',1)[0]+'/D00'
            elif edit=='root':rows[0]['path']=I.PAIR[0]+'/root-renamed'
            else:next(e for e in rows if e['path'].endswith('/d00'))['kind']='file'
            cases.append((edit,v))
        for name,value in cases:
            with self.subTest(name=name),self.assertRaises(Refused):self.parse(value)

    def test_metadata_refuses_invalid_each_field(self):
        bad={'kind':'symlink','mode':True,'uid':-1,'gid':2**32,'mtime_ns':2**63,
             'windows_attributes':1024,'nlink':2,'bytes':8388609,'sha256':'g'*64}
        for field,value in bad.items():
            v=copy.deepcopy(self.value);row=next(e for e in v['entries'] if e['kind']=='file');row[field]=value
            with self.subTest(field=field),self.assertRaises(Refused):self.parse(v)
        for field,value in [('bytes',1),('sha256','0'*64)]:
            v=copy.deepcopy(self.value);v['entries'][0][field]=value
            with self.assertRaises(Refused):self.parse(v)

    def test_logical_alias_and_ustar_limits(self):
        for suffix in ['/../x','//x','/x/','/x\\y','/x:y','/x.','/x ','/x\x00','/e\u0301','/'+('x'*513)]:
            with self.subTest(suffix=suffix),self.assertRaises(Refused):I.logical(I.PAIR[0]+suffix)
        v=copy.deepcopy(self.value);v['entries'][0]['uid']=2**32-1
        with self.assertRaisesRegex(Refused,'ustar'):self.parse(v)

    def test_strict_codec_duplicate_noncanonical_depth_and_byte_bound(self):
        for raw in [b'{"x":1,"x":2}', b' '+wire(self.value), b'['*25+b'0'+b']'*25,
                    b'x'*(1048576+1), b'{"x":1.0}']:
            with self.assertRaises(Refused):I.plan(raw,h(raw),self.plan.original,self.plan.physical)
        with self.assertRaisesRegex(Refused,'pin'):I.plan(wire(self.value),'0'*64,self.plan.original,self.plan.physical)

    def test_archive_staging_equality_overflow_and_no_extra_reserve(self):
        size=self.plan.pack_bytes
        exact=self.parse(physical=I.Physical(4095,67108864-size))
        self.assertEqual(exact.final.nodes,4012)
        for physical in [I.Physical(4096,2000000),I.Physical(3500,67108864-size+1),
                         I.Physical(83,2000000),I.Physical(3500,224851),I.Physical(True,224852)]:
            with self.subTest(physical=physical),self.assertRaises(Refused):self.parse(physical=physical)

    def test_composite_all_none_and_partial_originals(self):
        names=tuple(e.path for e in self.plan.entries)
        for present in [names,(),names[:11],names[:83]]:
            held=sum(e.bytes for e in self.plan.entries if e.path in present)
            physical=I.Physical(self.plan.final.nodes+len(present), self.plan.final.bytes+held)
            self.assertEqual(I.composite(self.plan,physical,present),self.plan.physical)
        for present in [(names[0],names[0]),('outside',)]:
            with self.assertRaises(Refused):I.composite(self.plan,self.plan.physical,present)
        with self.assertRaises(Refused):I.composite(self.plan,I.Physical(0,0),())

    def test_plan_and_context_forgery_refuse(self):
        with self.assertRaises(Refused):I.validate_plan(replace(self.plan,pack_bytes=self.plan.pack_bytes+512))
        _,session,_,_=context()
        session.expected_pin='0'*64
        with self.assertRaises(Refused):I.Operation(self.plan,session)
        with self.assertRaises(Refused):I.Operation(self.plan,object())

    def test_source_identity_alias_refuses_before_first_write(self):
        source=Source(self.value,self.data);source.alias=True
        _,session,_,_=context();port=Port()
        with self.assertRaisesRegex(Refused,'identity-alias'):I.Operation(self.plan,session).build(source,port)
        self.assertEqual(port.raw,b'')
        self.assertTrue(session.poisoned)

    def test_destination_nonempty_or_source_alias_refuses(self):
        for kind in ['nonempty','alias']:
            source=Source(self.value,self.data);_,session,_,_=context();port=Port(b'x' if kind=='nonempty' else b'')
            if kind=='alias':port.identity=h(self.plan.entries[0].path.encode())
            before=bytes(port.raw)
            with self.assertRaises(Refused):I.Operation(self.plan,session).build(source,port)
            self.assertEqual(bytes(port.raw),before)

    def test_source_hash_short_suffix_and_metadata_change(self):
        for kind in ['hash','short','suffix','metadata']:
            source=Source(self.value,self.data);name=next(n for n,b in source.data.items() if b)
            if kind=='hash':source.data[name]=b'x'*len(source.data[name])
            elif kind=='short':source.data[name]=source.data[name][:-1]
            elif kind=='suffix':source.data[name]+=b'x'
            else:
                original=source.read_exact_at
                def change(n,o,z):
                    raw=original(n,o,z);source.rows[n]=replace(source.rows[n],mode=0o600);return raw
                source.read_exact_at=change
            _,session,_,_=context();port=Port()
            with self.subTest(kind=kind),self.assertRaises(Refused):I.Operation(self.plan,session).build(source,port)
            self.assertTrue(session.poisoned)

    def test_partial_unknown_bool_write_preserves_effect_and_blocks_retry(self):
        for fault in ['short','unknown','bool']:
            _,session,store,_=context();port=Port();port.fault=fault;op=I.Operation(self.plan,session)
            with self.subTest(fault=fault),self.assertRaises(Refused) as failure:op.build(Source(self.value,self.data),port)
            self.assertNotIn('secret',str(failure.exception))
            self.assertGreater(len(port.raw),0)
            previous=bytes(port.raw)
            with self.assertRaises(Refused):op.build(Source(self.value,self.data),port)
            self.assertEqual(bytes(port.raw),previous)
            self.assertTrue(json.loads(store.raw)['denied'])
            with self.assertRaises(Refused):I.Operation(self.plan,session)

    def test_sync_unacknowledged_and_exception_keep_complete_partial_image(self):
        for fault in ['sync','sync-error']:
            _,session,_,_=context();port=Port();port.fault=fault
            with self.assertRaises(Refused):I.Operation(self.plan,session).build(Source(self.value,self.data),port)
            self.assertEqual(bytes(port.raw),self.raw)
            self.assertTrue(session.poisoned)

    def test_corruption_at_header_payload_padding_index_and_tail(self):
        # Locate members independently using tarfile, then change bytes and
        # recompute the outer pin. The inner byte/metadata oracles must refuse.
        with tarfile.open(fileobj=io.BytesIO(self.raw),mode='r:') as archive:
            members=archive.getmembers();file=next(m for m in members if m.isfile() and m.size==70000)
            index=members[-1]
            offsets=[0,file.offset_data,file.offset_data+file.size,index.offset_data,len(self.raw)-1]
        for offset in offsets:
            raw=bytearray(self.raw);raw[offset]^=1
            op,port,_,_=self.fresh(bytes(raw))
            with self.subTest(offset=offset),self.assertRaises(Refused):op.open(port,h(raw))

    def test_truncation_appended_bytes_and_wrong_outer_hash(self):
        for raw,expected in [(self.raw[:-1],None),(self.raw+b'x',None),(self.raw,'0'*64)]:
            op,port,_,_=self.fresh(raw)
            with self.assertRaises(Refused):op.open(port,h(raw) if expected is None else expected)

    def test_port_short_overread_and_revision_change(self):
        for fault in ['read-short','read-long','revision']:
            op,port,_,_=self.fresh()
            if fault=='revision':
                read=port.read_exact_at
                def change(o,n):
                    raw=read(o,n);port.revision+=1;return raw
                port.read_exact_at=change
            else:port.fault=fault
            with self.subTest(fault=fault),self.assertRaises(Refused):op.open(port,h(self.raw))

    def test_expiry_during_write_keeps_effect_and_original_context(self):
        _,session,store,clock=context();port=Port();write=port.write_exact_at
        def late(offset,raw):
            n=write(offset,raw);clock.tick=1000100;return n
        port.write_exact_at=late;pin=session.expected_pin
        with self.assertRaisesRegex(Refused,'original-deadline'):I.Operation(self.plan,session).build(Source(self.value,self.data),port)
        self.assertGreater(len(port.raw),0)
        self.assertEqual(session.expected_pin,pin)
        self.assertTrue(json.loads(store.raw)['denied'])

    def test_failed_cas_and_audit_rescan_caps_do_not_reset(self):
        for kind in ['ack','audit','rescans']:
            op,port,store,_=self.fresh()
            if kind=='ack':store.ack=False
            else:
                state=json.loads(store.raw);state['credits']['audit_bytes' if kind=='audit' else 'rescans']=268435456 if kind=='audit' else 32
                store.raw=wire(state)
            with self.subTest(kind=kind),self.assertRaises(Refused):op.open(port,h(self.raw))
            self.assertEqual(port.calls,[])

    def test_range_refusals_are_sticky(self):
        name=next(n for n,b in self.data.items() if len(b)==70000)
        for args in [(name,h(self.data[name]),0,65537),(name,h(self.data[name]),70000,1),
                     (name,'0'*64,0,1),(self.plan.entries[0].path,'0'*64,0,1),('outside','0'*64,0,1)]:
            op,port,_,_=self.fresh();view=op.open(port,h(self.raw))
            with self.subTest(args=args),self.assertRaises(Refused):view.read(*args)
            with self.assertRaises(Refused):view.entries()

    def test_member_change_after_open_rehashes_before_return(self):
        op,port,_,_=self.fresh();view=op.open(port,h(self.raw))
        name=next(n for n,b in self.data.items() if len(b)==70000)
        # Keep the mock revision dishonest on purpose. Member hashing must still
        # refuse corruption outside the requested slice.
        port.raw[view._offsets[name]+69999]^=1
        with self.assertRaisesRegex(Refused,'member-hash'):view.read(name,h(self.data[name]),0,1)

    def test_view_stamp_identity_and_forged_constructor(self):
        op,port,_,_=self.fresh();view=op.open(port,h(self.raw));port.identity='0'*64
        with self.assertRaises(Refused):view.entries()
        with self.assertRaises(Refused):I.View(op,port,port.describe(),{},self.evidence,None)

    def test_whole_build_debits_original_shared_credits(self):
        _,session,store,_=context();port=Port();source=Source(self.value,self.data)
        I.Operation(self.plan,session).build(source,port)
        state=json.loads(store.raw)
        self.assertEqual(state['credits']['rescans'],5)
        self.assertEqual(state['credits']['raw_bytes'],0)
        self.assertEqual(state['credits']['raw_reservations'],0)
        self.assertEqual(source.calls,336)
        self.assertGreater(state['credits']['audit_bytes'],224852+len(self.raw)*2)
        self.assertLessEqual(state['credits']['audit_bytes'],192507546)
        _,session,store,_=context();state=json.loads(store.raw);state['credits']['rescans']=29;store.raw=wire(state)
        source=Source(self.value,self.data);port=Port()
        with self.assertRaises(Refused):I.Operation(self.plan,session).build(source,port)
        self.assertEqual(source.calls,0)
        self.assertEqual(port.calls,[])

    def test_port_refusal_diagnostics_are_redacted(self):
        op,port,_,_=self.fresh()
        def fail():raise Refused('sensitive synthetic payload')
        port.describe=fail
        with self.assertRaisesRegex(Refused,'^image-port-uncertain$'):op.open(port,h(self.raw))

    def test_reentrant_port_cannot_start_a_second_operation(self):
        op,port,_,_=self.fresh();read=port.read_exact_at;attempts=[]
        def reenter(offset,length):
            with self.assertRaisesRegex(Refused,'image-concurrent'):
                op.open(port,h(self.raw))
            attempts.append(True)
            return read(offset,length)
        port.read_exact_at=reenter
        view=op.open(port,h(self.raw))
        self.assertTrue(attempts)
        self.assertEqual(view.evidence.pack_sha256,h(self.raw))

    def test_hidden_suffix_eof_and_untyped_empty_eof_refuse(self):
        for fault in ['suffix','untyped']:
            op,port,_,_=self.fresh();read=port.read_exact_at
            def hidden(offset,length):
                if offset==len(self.raw):return b'x' if fault=='suffix' else bytearray()
                return read(offset,length)
            port.read_exact_at=hidden
            with self.assertRaisesRegex(Refused,'image-pack-eof'):op.open(port,h(self.raw))

    def test_production_has_no_controller_host_io_or_clock_construction(self):
        tree=ast.parse(pathlib.Path(I.__file__).read_text())
        imports=[]
        for node in ast.walk(tree):
            if isinstance(node,ast.Import):imports.extend(n.name for n in node.names)
            elif isinstance(node,ast.ImportFrom):imports.append(node.module)
            elif isinstance(node,ast.Call) and isinstance(node.func,ast.Name):
                self.assertNotIn(node.func.id,{'open','eval','exec','__import__'})
        self.assertEqual(set(imports),{'dataclasses','hashlib','pathlib','tarfile','threading','unicodedata',
                                      'typing','tools.retention_context_v1.codec','tools.retention_context_v1.context'})


if __name__ == '__main__':
    unittest.main()
