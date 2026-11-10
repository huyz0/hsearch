"""HOST/synthetic controls only. No real S3/TLS/credential/resource operation."""
import copy
import hashlib
import io
import json
from pathlib import Path
import runpy
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

HERE=Path(__file__).resolve().parent
D=runpy.run_path(str(HERE/'object_workload.py'))
V=runpy.run_path(str(HERE/'object_sample_verify.py'))

class Clock:
    def __init__(self,yielding=True):self.value=1_000_000_000;self.lock=threading.Lock();self.yielding=yielding
    def now(self):
        with self.lock:self.value+=1000;return self.value
    def until(self,deadline):
        with self.lock:self.value=max(self.value,deadline)
        if self.yielding:time.sleep(.0001)

class Store:
    """Atomic mock storage deliberately separate from the independent oracle."""
    def __init__(self,delay=0):self.data={};self.lock=threading.Lock();self.tags=0;self.barriers={};self.calls=0;self.delay=delay
    def sensitive(self,_):return False
    def exchange(self,method,path,body,condition,deadline):
        self.calls+=1
        if self.delay:time.sleep(self.delay)
        if path=='/health/ready':return D['Reply'](None,None,b'','TLSCertificate',0)
        if path=='/':return D['Reply'](403,None,b'<Error><Code>SignatureDoesNotMatch</Code></Error>','SignatureDoesNotMatch',0)
        if path.endswith('/cas') and method=='PUT' and 'if-match' in condition:
            try:value=json.loads(body)
            except ValueError:value={}
            if set(value)=={'round','generation','contender'} and 0<=value['round']<8:
                with self.lock:barrier=self.barriers.setdefault(value['round'],threading.Barrier(4))
                barrier.wait(timeout=10)
        with self.lock:
            if method=='PUT':
                old=self.data.get(path)
                bad=condition.get('if-none-match')=='*' and old is not None or 'if-match' in condition and (old is None or old[1]!=condition['if-match'])
                if bad:return D['Reply'](412,None,b'<Error><Code>PreconditionFailed</Code></Error>','PreconditionFailed',len(body))
                self.tags+=1;tag='"opaque-'+str(self.tags)+'"';self.data[path]=(bytes(body),tag)
                return D['Reply'](200,tag,b'',None,len(body))
            if path not in self.data:return D['Reply'](404,None,b'<Error><Code>NoSuchKey</Code></Error>','NoSuchKey',0)
            body,tag=self.data[path];return D['Reply'](200,tag,body,None,0)


def binding():
    b={key:'0'*64 for key in D['BINDING_FIELDS']}
    b.update(run_id='1'*32,source_head='2'*40,source_tree='3'*40,target='x86_64-unknown-linux-gnu',execution_kind='synthetic')
    b['fixture_sha256']=hashlib.sha256(D['FIXTURE'].read_bytes()).hexdigest()
    b['lock_sha256']=hashlib.sha256((D['ROOT']/'Cargo.lock').read_bytes()).hexdigest()
    b['driver_sha256']=hashlib.sha256((HERE/'object_workload.py').read_bytes()).hexdigest()
    b['oracle_sha256']=hashlib.sha256((HERE/'object_sample_verify.py').read_bytes()).hexdigest()
    b['plan_sha256']=hashlib.sha256(D['canonical'](D['plan']())).hexdigest()
    return b

def encode(rows):
    return b''.join(D['canonical']({**r,'seq':i})+b'\n' for i,r in enumerate(rows))

def make_case(directory,*,trace=None,group='compatible',wait=0,path='root-cas',number=0,clock=None,store=None,budget=None,export=None,b=None):
    trace=trace or D['MODEL']['fixtures']()['traces'][3]
    clock=clock or Clock();store=store or Store();budget=budget or D['Budget'](clock);export=export or D['Export']();b=b or binding()
    j=D['Journal'](Path(directory)/('case-'+str(number).zfill(3)+'.jsonl'),export)
    j.emit('capture_started',schema=1,binding=b,case_number=number,case=D['case_identity'](trace,group,wait,path))
    c=D['Client'](store,budget,j,clock,clock.now()+D['CASE_NS'])
    result=D['execute_case'](c,j,clock,trace,group,wait,path,'/hsearch-mock/'+b['run_id']+'/case-'+str(number).zfill(3))
    descriptor=j.close();return j.path.read_bytes(),result,(trace,group,wait,path,number,b,'hsearch-mock'),descriptor

class ObjectOracle(unittest.TestCase):
    def setUp(self):self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name)
    def tearDown(self):self.temp.cleanup()
    def verify(self,raw,args):return V['case_verify'](raw,*args)
    def test_both_comparison_paths_and_checkpoint_content(self):
        for n,path in enumerate(('slots','root-cas')):
            raw,result,args,_=make_case(self.root,path=path,number=n)
            self.assertTrue(result['healthy']);verified=self.verify(raw,args)
            self.assertEqual(verified['offered'],128);self.assertEqual(verified['committed'],128)
            records=V['rows'](raw);self.assertTrue(any(r.get('stage')=='checkpoint-page' for r in records))
            if path=='root-cas':self.assertTrue(any(r.get('stage')=='root-update' for r in records))
            else:self.assertFalse(any(r.get('stage')=='root-update' for r in records))
    def test_drop_duplicate_unknown_and_closed_loop_records_are_refused(self):
        raw,_,args,_=make_case(self.root)
        original=V['rows'](raw)
        variants=[]
        variants.append([r for r in original if not (r['kind']=='event_finished' and r['id']==args[0]['events'][0]['id'])])
        variants.append(original+[next(r for r in original if r['kind']=='event_finished')])
        bad=copy.deepcopy(original);next(r for r in bad if r['kind']=='event_offered')['planned_ns']+=1;variants.append(bad)
        bad=copy.deepcopy(original);next(r for r in bad if r['kind']=='event_finished')['status']='unknown';variants.append(bad)
        bad=copy.deepcopy(original);next(r for r in bad if r['kind']=='event_admitted')['pending']+=1;variants.append(bad)
        for variant in variants:
            with self.assertRaises((V['Invalid'],ValueError)):self.verify(encode(variant),args)
    def test_wrong_data_tag_root_path_or_unconditional_write_is_refused(self):
        raw,_,args,_=make_case(self.root);original=V['rows'](raw);variants=[]
        for field,value in [('key','/hsearch-other/wrong'),('condition',{}),('body_sha256','a'*64)]:
            bad=copy.deepcopy(original);next(r for r in bad if r.get('stage')=='payload')[field]=value;variants.append(bad)
        bad=copy.deepcopy(original);started=next(r for r in bad if r.get('stage')=='verify-payload');finished=next(r for r in bad if r['kind']=='request_finished' and r['request']==started['request']);finished['response_sha256']='f'*64;variants.append(bad)
        bad=copy.deepcopy(original);root=next(r for r in bad if r.get('stage')=='root-update');finished=next(r for r in bad if r['kind']=='request_finished' and r['request']==root['request']);finished['etag']='"stale"';variants.append(bad)
        for variant in variants:
            with self.assertRaises((V['Invalid'],ValueError)):self.verify(encode(variant),args)
    def test_hidden_retry_and_wrong_binding_do_not_qualify(self):
        raw,_,args,_=make_case(self.root);records=V['rows'](raw)
        first=next(r for r in records if r['kind']=='request_started');end=next(r for r in records if r['kind']=='request_finished' and r['request']==first['request'])
        a=copy.deepcopy(first);z=copy.deepcopy(end);a['request']=z['request']=32768;a['start_ns']=z['end_ns']+1;z['end_ns']=a['start_ns']+1
        with self.assertRaises((V['Invalid'],ValueError)):self.verify(encode(records+[a,z]),args)
        bad=copy.deepcopy(records);bad[0]['binding']['image_sha256']='a'*64
        with self.assertRaises(V['Invalid']):self.verify(encode(bad),args)
    def test_timer_and_final_drain_are_observed_not_early_flush(self):
        raw,_,args,_=make_case(self.root,wait=250000)
        self.verify(raw,args);bad=V['rows'](raw)
        batch=next(r for r in bad if r['kind']=='batch_started');batch['started_ns']=0
        with self.assertRaises(V['Invalid']):self.verify(encode(bad),args)
    def test_queue_overload_remains_failed_with_full_event_conservation(self):
        trace=D['MODEL']['fixtures']()['traces'][6]
        raw,result,args,_=make_case(self.root,trace=trace,clock=Clock(False),store=Store(.002))
        self.assertFalse(result['healthy']);self.assertEqual(result['committed']+result['unknown']+result['rejected'],128)
        with self.assertRaises(V['Invalid']):self.verify(raw,args)
    def test_every_prefix_truncation_and_oversize_or_duplicate_json(self):
        tiny=b'{"seq":0,"kind":"x"}\n'
        for end in range(len(tiny)):
            with self.assertRaises(V['Invalid']):V['rows'](tiny[:end])
        for raw in (b'{"a":1,"a":2}',b'['*17+b']'*17,b'{"a":NaN}',b'x'*(V['MAX_LINE']+1)):
            with self.assertRaises(V['Invalid']):V['decode'](raw)
    def test_attempt_budget_plus_one_refuses_before_transport(self):
        clock=Clock();budget=D['Budget'](clock);budget.attempts=D['MAX_ATTEMPTS'];store=Store();j=D['Journal'](self.root/'bound.jsonl',D['Export']())
        client=D['Client'](store,budget,j,clock,clock.now()+D['CASE_NS'])
        try:
            with self.assertRaises(D['Refused']):client.call('payload',[0],'/hsearch-mock/x','PUT',b'x',{'if-none-match':'*'})
            self.assertEqual(store.calls,0);self.assertEqual(j.reserved,0)
            self.assertEqual(V['rows'](j.path.read_bytes())[-1]['kind'],'admission_refused')
        finally:j.file.close()
    def test_sigv4_signs_conditions_and_keeps_them_out_of_records(self):
        a=D['sign']('PUT','/hsearch-mock/a',b'x','fake-access','fake-secret','20260101T000000Z',{'if-match':'"opaque"'})
        b=D['sign']('PUT','/hsearch-mock/a',b'x','fake-access','fake-secret','20260101T000000Z',{'if-match':'"different"'})
        self.assertIn('if-match',a['authorization']);self.assertNotEqual(a['authorization'],b['authorization'])
        raw,_,_args,_=make_case(self.root)
        self.assertNotIn(b'authorization',raw.lower());self.assertNotIn(b'fake-secret',raw)

    def test_encoded_credentials_are_redacted_before_artifact_output(self):
        transport=D['S3'].__new__(D['S3']);transport.access='fake-access';transport.secret='fake-secret'
        self.assertTrue(transport.sensitive(b'<Error><Message>fake&#45;secret</Message></Error>'))
        self.assertTrue(transport.sensitive(b'{"message":"fake\\u002dsecret"}'))
        self.assertTrue(transport.sensitive(b'fake%2Dsecret'))
        class Echo(Store):
            def sensitive(self,raw):return transport.sensitive(raw)
            def exchange(self,*_):return D['Reply'](403,None,b'<Error><Message>fake&#45;secret</Message><Code>AccessDenied</Code></Error>','AccessDenied',0)
        clock=Clock();j=D['Journal'](self.root/'redaction.jsonl',D['Export']())
        r=D['Client'](Echo(),D['Budget'](clock),j,clock,clock.now()+D['CASE_NS']).call('credential-negative',[],'/','GET')
        j.close();records=V['rows'](j.path.read_bytes());self.assertTrue(records[-1]['redacted'])
        self.assertIsNone(records[-1]['response_metadata_hex']);self.assertNotIn(b'fake',j.path.read_bytes())
    def test_allocation_input_bounds_are_checked_before_decode(self):
        path=self.root/'large';path.write_bytes(b'a'*1025)
        with self.assertRaises(D['Refused']):D['read_bounded'](path,1024)
        with self.assertRaises(V['Invalid']):V['read_file'](path,1024)

class FaultOracle(unittest.TestCase):
    def setUp(self):self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name)
    def tearDown(self):self.temp.cleanup()
    def capture(self):
        clock=Clock();b=binding();j=D['Journal'](self.root/'faults.jsonl',D['Export']());j.emit('capture_started',schema=1,binding=b,case_number='faults')
        D['execute_faults'](D['Client'](Store(),D['Budget'](clock),j,clock,clock.now()+D['CASE_NS']),j,clock,'/hsearch-mock/'+b['run_id']+'/faults')
        j.close();return j.path.read_bytes(),b
    def test_actual_mock_cas_lineage_loss_resolution_and_orphan(self):
        raw,b=self.capture();result=V['faults_verify'](raw,b,'hsearch-mock')
        self.assertEqual((result['cas_rounds'],result['cas_winners'],result['cas_conflicts']),(8,8,24))
        self.assertEqual(result['actual_requests'],60)
    def test_two_winners_stale_tag_false_overlap_and_unknown_rollback_refused(self):
        raw,b=self.capture();original=V['rows'](raw);variants=[]
        r=next(x for x in original if x['kind']=='cas_round');request=r['contenders'][0]
        bad=copy.deepcopy(original);loser=next(x for x in bad if x['kind']=='request_finished' and x['request'] in r['contenders'] and x['status']!=200)
        loser.update(status=200,error=None,etag='"second-winner"',response_bytes=0,response_sha256=D['digest'](b''),response_metadata_hex='');variants.append(bad)
        bad=copy.deepcopy(original);next(x for x in bad if x['kind']=='request_started' and x['request']==request)['condition']={'if-match':'"old"'};variants.append(bad)
        bad=copy.deepcopy(original)
        starts={x['request']:x['start_ns'] for x in bad if x['kind']=='request_started' and x['request'] in r['contenders']}
        earliest=min(starts,key=starts.get)
        first=next(x for x in bad if x['kind']=='request_finished' and x['request']==earliest);first['end_ns']=starts[earliest]+1
        self.assertLess(first['end_ns'],max(starts.values()),'Counterexample must actually eliminate common overlap')
        variants.append(bad)
        bad=copy.deepcopy(original);next(x for x in bad if x['kind']=='response_suppressed')['caller_outcome']='rolled-back';variants.append(bad)
        bad=copy.deepcopy(original);next(x for x in bad if x['kind']=='loss_resolved')['original_request']+=1;variants.append(bad)
        bad=copy.deepcopy(original);next(x for x in bad if x['kind']=='faults_finished')['modeled_data_wrapping_units']=1;variants.append(bad)
        for number,variant in enumerate(variants):
            with self.subTest(mutation=number):
                with self.assertRaises(V['Invalid']):V['faults_verify'](encode(variant),b,'hsearch-mock')

class TransportBounds(unittest.TestCase):
    def exchange(self,wire):
        class Sock:
            def __enter__(self):return self
            def __exit__(self,*_):pass
            def connect(self,address):self.address=address
            def sendall(self,raw):self.sent=raw
            def settimeout(self,_):pass
            def makefile(self,*_,**__):return io.BytesIO(wire)
        sock=Sock()
        class Context:
            def wrap_socket(self,raw,server_hostname):self.name=server_hostname;return raw
        transport=D['S3'].__new__(D['S3']);transport.clock=Clock();transport.access='fake-access';transport.secret='fake-secret';transport.signing_secret='fake-secret';transport.context=Context();transport.address='172.18.0.2'
        with patch.object(D['socket'],'socket',return_value=sock):return transport.exchange('GET','/hsearch-mock/a',b'',{},transport.clock.now()+10**10)
    def test_wire_digest_tag_and_bounded_framing(self):
        r=self.exchange(b'HTTP/1.1 200 OK\r\nContent-Length: 3\r\nETag: "abc"\r\n\r\nxyz')
        self.assertEqual((r.status,r.body,r.etag),(200,b'xyz','"abc"'))
        for raw in (b'HTTP/1.1 200 OK\r\nContent-Length: 16385\r\n\r\n',
                    b'HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\nxyz',
                    b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n',
                    b'HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nx',
                    b'HTTP/1.1 200 OK\r\nX: '+b'a'*2049+b'\r\n\r\n'):
            self.assertEqual(self.exchange(raw).error,'TransportOrFraming')
    def test_xml_entities_and_excess_depth_refused(self):
        for raw in (b'<!DOCTYPE x><Error><Code>PreconditionFailed</Code></Error>',b'<x>'*10+b'</x>'*10):
            with self.assertRaises(D['Refused']):D['error_code'](raw)

class FullSyntheticMatrix(unittest.TestCase):
    def test_all144_mock_cases_and_independent_bound_export_oracle(self):
        # This executes MOCK operations only. Never relabel the resulting
        # synthetic receipt as 144 RustFS/provider cases.
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);clock=Clock();store=Store();budget=D['Budget'](clock);export=D['Export']();b=binding();descriptors=[]
            run=D['Journal'](root/'run.jsonl',export)
            runtime={'uid':10001,'gid':10001,'architecture':'x86_64','caps':{'cpu.max':'200000 100000','memory.max':str(4*1024**3),'memory.swap.max':'0','pids.max':'256'},
                     'clock':'monotonic_ns','scratch_bytes':18*1024**3,'scratch_proof':'synthetic only'}
            run.emit('run_started',schema=1,binding=b,bucket='hsearch-mock',runtime=runtime,started_ns=clock.now(),plan=D['plan']())
            c=D['Client'](store,budget,run,clock,clock.now()+D['CASE_NS'])
            c.call('tls-negative',[],'/health/ready','GET');c.call('credential-negative',[],'/','GET');c.call('bucket',[],'/hsearch-mock','PUT')
            for number,(trace,group,wait,path) in enumerate(D['cases']()):
                raw,result,args,descriptor=make_case(root,trace=trace,group=group,wait=wait,path=path,number=number,clock=clock,store=store,budget=budget,export=export,b=b)
                self.assertTrue(result['healthy'],args[:5]);V['case_verify'](raw,*args);descriptors.append(descriptor)
            faults=D['Journal'](root/'faults.jsonl',export);faults.emit('capture_started',schema=1,binding=b,case_number='faults')
            D['execute_faults'](D['Client'](store,budget,faults,clock,clock.now()+D['CASE_NS']),faults,clock,'/hsearch-mock/'+b['run_id']+'/faults');descriptors.append(faults.close())
            run.emit('run_finished',finished_ns=clock.now(),completed_cases=144,attempts=budget.attempts,admission_body_bytes=budget.body_bytes,
                     known_sent_body_bytes=budget.known_sent_body_bytes,unknown_sent_attempts=budget.unknown_sends,peak_inflight=budget.peak,peak_process_rss_bytes=1024,
                     actual_kms_calls=0,scope='HOST mock only');descriptors.append(run.close())
            index={'schema':1,'binding':b,'files':descriptors,'total_bytes':sum(x['bytes'] for x in descriptors),'status':'complete','completed_cases':144,'healthy_cases':144,'binary_invocations':0,'scope':'synthetic only'}
            encoded=D['canonical'](index)+b'\n';(root/'index.json').write_bytes(encoded)
            execution={'kind':'synthetic','index_sha256':D['digest'](encoded),'bucket':'hsearch-mock','worker_cid':'a'*64,'image_sha256':b['image_sha256'],
                       'raw_worker_sha256':'b'*64,'runtime':runtime,'monitor':{'cpu_millis':2000,'memory_bytes':4*1024**3,'added_swap_bytes':0,'pids':256,
                       'scratch_bytes':18*1024**3,'seconds':1200,'output_bytes':1048576,'exit_code':0,'truncated':False},'export_files':descriptors}
            verified=V['verify'](root,b,execution);self.assertEqual(verified['actual_case_records'],144);self.assertEqual(verified['execution_kind'],'synthetic')
            self.assertLess(index['total_bytes'],32*1024**2);self.assertLess(budget.attempts,32768)
            stricter=copy.deepcopy(execution);stricter['monitor']['output_bytes']=524288
            self.assertEqual(V['verify'](root,b,stricter)['actual_case_records'],144)
            excessive=copy.deepcopy(execution);excessive['monitor']['output_bytes']=1048577
            with self.assertRaises(V['Invalid']):V['verify'](root,b,excessive)
            for mutation in ('missing','binding','caps','index-count','raw-start','counter'):
                badindex=copy.deepcopy(index);badexec=copy.deepcopy(execution)
                if mutation=='missing':badindex['files']=badindex['files'][:-1];badexec['export_files']=badindex['files']
                elif mutation=='binding':badindex['binding']['source_sha256']='c'*64
                elif mutation=='caps':badexec['runtime']['caps']['memory.max']='max'
                elif mutation=='index-count':badindex['completed_cases']=72
                elif mutation in ('raw-start','counter'):
                    original=(root/'run.jsonl').read_bytes();rr=V['rows'](original)
                    if mutation=='raw-start':rr[0]['runtime']['uid']=0
                    else:rr[-1]['attempts']-=1
                    altered=encode(rr);(root/'run.jsonl').write_bytes(altered)
                    descriptor=next(x for x in badindex['files'] if x['file']=='run.jsonl');descriptor.update(bytes=len(altered),sha256=D['digest'](altered))
                    badindex['total_bytes']=sum(x['bytes'] for x in badindex['files']);badexec['export_files']=badindex['files']
                encoded=D['canonical'](badindex)+b'\n';(root/'index.json').write_bytes(encoded);badexec['index_sha256']=D['digest'](encoded)
                with self.assertRaises(V['Invalid']):V['verify'](root,b,badexec)
                if mutation in ('raw-start','counter'):(root/'run.jsonl').write_bytes(original)
                (root/'index.json').write_bytes(D['canonical'](index)+b'\n')

if __name__=='__main__':unittest.main(verbosity=2)
