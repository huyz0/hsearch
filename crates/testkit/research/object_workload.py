"""Private M02.10 object experiment, never production authority.

Fixed future guest entry: python3 /workspace/crates/testkit/research/object_workload.py
No arguments or environment selectors. The integrator must mount exactly
/object-input/binding.json read-only and a new /object-output, public /certs/ca.crt,
/certs/wrong-ca.crt and development /run/secrets/{access,secret}. The normal outer
monitor must admit the DISTINCT 2CPU/4GiB/256PID/18GiB/1200s object envelope,
private network, acknowledged CID/image/source and bounded export. This module
neither launches a host/container nor changes the existing 8GiB build worker.

Input schema1: {schema,binding,bucket}. binding has the closed BINDING_FIELDS;
all hashes are admitted upstream, never inferred from successful worker output.
Output: index.json, run.jsonl, faults.jsonl, case-000..143.jsonl. Each raw extent
<=1MiB, aggregate<=32MiB, <=256 files; stdout contains only an index hash/count.
The outer export supplies index bytes/hash, source/runtime/CID/monitor provenance
independently. object_sample_verify.verify checks exported records separately.
HOST synthetic clients do not establish actual HTTP/provider/physical evidence.
"""
from __future__ import annotations
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import hashlib
import hmac
import json
import ipaddress
import os
from pathlib import Path
import re
import runpy
import socket
import ssl
import sys
import threading
import time
from xml.etree import ElementTree

ROOT = Path(__file__).resolve().parents[3]
MODEL = runpy.run_path(str(ROOT / 'tools/durability_research.py'))
FIXTURE = ROOT / MODEL['FIXTURE']
PATHS = ('slots', 'root-cas')
BINDING_FIELDS = {'run_id','source_head','source_tree','source_sha256','lock_sha256',
                  'fixture_sha256','envelope_sha256','image_sha256','topology_sha256',
                  'config_sha256','driver_sha256','oracle_sha256','plan_sha256','target','execution_kind'}
MAX_FILE = 1024*1024
MAX_EXPORT = 32*1024*1024
MAX_LINE = 32768
MAX_RECORDS = 4096
MAX_ATTEMPTS = 32768
MAX_BODY_TOTAL = 64*1024*1024
CASE_NS = 15_000_000_000
COMMAND_NS = 1_200_000_000_000
BODY_CAP = 16384
META_CAP = 4096
GLOBAL_PENDING = 512
LANE_PENDING = 32
PAYLOAD_RESIDENT = 512*1024
STAGES = {'bucket','tls-negative','credential-negative','activation','slot','payload',
          'root-read','root-update','verify-payload','checkpoint-page','checkpoint-root',
          'fault-create','fault-read','fault-cas','fault-stale','fault-retry','fault-occupied',
          'fault-loss','fault-resolve','fault-orphan','fault-root-reject'}

class Refused(ValueError):
    """Content-free refusal. No exception includes credentials or signed headers."""

def need(ok, message):
    if not ok: raise Refused(message)

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',',':'), ensure_ascii=True).encode('ascii')

def digest(raw): return hashlib.sha256(raw).hexdigest()

def read_bounded(path,cap):
    path=Path(path);need(path.is_file() and not path.is_symlink(),'Bounded input file')
    before=path.stat();need(before.st_nlink==1 and 0<before.st_size<=cap,'Input extent bound')
    with path.open('rb') as f:raw=f.read(cap+1)
    after=path.stat();need(len(raw)==before.st_size and len(raw)<=cap and
        (before.st_ino,before.st_size,before.st_mtime_ns)==(after.st_ino,after.st_size,after.st_mtime_ns),'Input changed')
    return raw

def decode(raw, cap=META_CAP):
    need(type(raw) is bytes and len(raw)<=cap, 'JSON extent bound')
    depth=0; string=False; escaped=False
    for c in raw:
        if string:
            if escaped: escaped=False
            elif c==92: escaped=True
            elif c==34: string=False
        elif c==34: string=True
        elif c in (91,123):
            depth+=1;need(depth<=16,'JSON depth bound')
        elif c in (93,125): depth-=1;need(depth>=0,'JSON framing')
    need(not string and depth==0,'JSON framing')
    def pairs(items):
        need(len(items)<=256 and len(dict(items))==len(items),'JSON keys')
        return dict(items)
    try: value=json.loads(raw,object_pairs_hook=pairs,parse_float=lambda _: (_ for _ in ()).throw(Refused('JSON float')),parse_constant=lambda _: (_ for _ in ()).throw(Refused('JSON number')))
    except (ValueError,UnicodeError,RecursionError): raise Refused('JSON encoding') from None
    return value

def binding_check(binding):
    need(type(binding) is dict and set(binding)==BINDING_FIELDS,'Binding schema')
    need(re.fullmatch('[0-9a-f]{32}',binding['run_id']) is not None,'Run identity')
    for key in ('source_head','source_tree'):
        need(type(binding[key]) is str and re.fullmatch('[0-9a-f]{40}',binding[key]),'Source identity')
    for key in BINDING_FIELDS-{'run_id','source_head','source_tree','target','execution_kind'}:
        need(type(binding[key]) is str and re.fullmatch('[0-9a-f]{64}',binding[key]),'Binding digest')
    need(binding['target'] in ('x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu'),'Native target')
    need(binding['execution_kind'] in ('synthetic','admitted-object'),'Execution kind')

def cases():
    return [(trace,group,wait,path) for trace in MODEL['fixtures']()['traces']
            for group in MODEL['GROUPINGS'] for wait in MODEL['WAIT_US'] for path in PATHS]

def plan():
    return {'schema':1,'matrix':[[t['workload'],t['seed'],g,w,p] for t,g,w,p in cases()],
            'payload_bytes':1024,'batch_bytes':16384,'lane_records':32,'lane_bytes':32768,
            'lanes':16,'pending_records':512,'resident_payload_bytes':PAYLOAD_RESIDENT,
            'requests':4,'request_per_lane':1,'checkpoint_batches':16,
            'case_ns':CASE_NS,'command_ns':COMMAND_NS,'attempts':MAX_ATTEMPTS,
            'body_bytes':MAX_BODY_TOTAL,'file_bytes':MAX_FILE,'export_bytes':MAX_EXPORT,
            'wrapping_model':{'data_object_units':1,'metadata_units':None,'actual_kms_calls':0},
            'faults':'8fresh-tag-rounds/4distinct-contenders;known-conflict-get+retry;stale;occupied;response-suppression;orphan'}

class Clock:
    def now(self): return time.monotonic_ns()
    def until(self, deadline):
        remaining=deadline-self.now()
        if remaining>0: time.sleep(remaining/1e9)

class Journal:
    """Reserve the completion record before effect; incomplete starts stay visible."""
    def __init__(self, path, export):
        need(path.parent.is_dir() and not path.exists() and not path.is_symlink(),'Fresh raw extent required')
        self.path=path;self.file=path.open('xb',buffering=0);self.export=export
        self.lock=threading.RLock();self.bytes=0;self.rows=0;self.reserved=0;self.finished=False
    def reserve(self, amount):
        with self.lock:
            need(self.bytes+self.reserved+amount<=MAX_FILE and self.rows<MAX_RECORDS-1,'Raw effect reservation bound')
            self.export.reserve(amount);self.reserved+=amount
    def emit(self, kind, **data):
        with self.lock:
            row={'seq':self.rows,'kind':kind,**data};raw=canonical(row)+b'\n'
            need(len(raw)<=MAX_LINE and self.rows<MAX_RECORDS and self.bytes+self.reserved+len(raw)<=MAX_FILE,'Raw record bound')
            self.export.consume(len(raw));need(self.file.write(raw)==len(raw),'Raw capture short write');self.bytes+=len(raw);self.rows+=1
    def complete(self, amount, kind, **data):
        with self.lock:
            need(self.reserved>=amount,'Raw reservation missing')
            self.reserved-=amount;self.export.release(amount)
            self.emit(kind,**data)
    def close(self):
        with self.lock:
            need(self.reserved==0,'Incomplete request record')
            self.file.flush();os.fsync(self.file.fileno());self.file.close();self.finished=True
        return {'file':self.path.name,'bytes':self.bytes,'sha256':digest(self.path.read_bytes())}

class Export:
    def __init__(self): self.lock=threading.RLock();self.used=0;self.reserved=0
    def reserve(self,n):
        with self.lock:
            need(self.used+self.reserved+n<=MAX_EXPORT,'Aggregate artifact bound');self.reserved+=n
    def release(self,n):
        with self.lock:self.reserved-=n
    def consume(self,n):
        with self.lock:
            need(self.used+self.reserved+n<=MAX_EXPORT,'Aggregate artifact bound');self.used+=n

class Budget:
    def __init__(self, clock):
        self.clock=clock;self.deadline=clock.now()+COMMAND_NS;self.lock=threading.Lock()
        self.attempts=0;self.body_bytes=0;self.known_sent_body_bytes=0;self.unknown_sends=0;self.reserved_response=0;self.active=0;self.peak=0;self.gate=threading.BoundedSemaphore(4)
    def enter(self, deadline, size):
        need(self.clock.now()<min(deadline,self.deadline),'Request deadline')
        with self.lock:
            need(self.attempts<MAX_ATTEMPTS and self.body_bytes+self.reserved_response+size+BODY_CAP<=MAX_BODY_TOTAL,'Aggregate HTTP admission bound')
            self.attempts+=1;self.body_bytes+=size;self.reserved_response+=BODY_CAP;self.active+=1;self.peak=max(self.peak,self.active)
            need(self.active<=4,'HTTP concurrency bound')
            return self.attempts
    def returned(self,n,sent):
        with self.lock:
            need(0<=n<=BODY_CAP,'HTTP response reservation');self.reserved_response-=BODY_CAP
            if sent is None:self.unknown_sends+=1
            else:self.known_sent_body_bytes+=sent
            self.body_bytes+=n;need(self.body_bytes<=MAX_BODY_TOTAL,'Aggregate HTTP body bound')
    def leave(self):
        with self.lock:self.active-=1

class Reply:
    def __init__(self,status,etag,body,error=None,sent=None):
        self.status=status;self.etag=etag;self.body=body;self.error=error;self.sent=sent

def xml_bound(raw):
    need(len(raw)<=META_CAP and b'<!' not in raw.upper(),'Error XML bound')
    depth=0;nodes=0
    for token in re.findall(rb'<([^<>]{1,2048})>',raw):
        if token.startswith(b'?'):continue
        if token.startswith(b'/'):depth-=1;need(depth>=0,'Error XML framing')
        else:
            nodes+=1;need(nodes<=64,'Error XML node admission')
            if not token.endswith(b'/'):depth+=1
            need(depth<=8,'Error XML depth admission')
    need(depth==0,'Error XML framing')

def error_code(raw):
    xml_bound(raw)
    try: root=ElementTree.fromstring(raw)
    except ElementTree.ParseError: return 'MalformedError'
    stack=[(root,0)];count=0
    while stack:
        node,depth=stack.pop();count+=1;need(count<=64 and depth<=8,'Error XML nodes')
        need(len(node.attrib)<=8 and len(node.text or '')<=1024,'Error XML field bound')
        stack.extend((child,depth+1) for child in node)
    value=root.findtext('Code')
    return value if type(value) is str and re.fullmatch('[A-Za-z0-9]{1,64}',value) else 'MalformedError'

def sign(method,path,body,access,secret,stamp,condition):
    """SigV4 variant of inspected dev/probe.py; conditions are signed too."""
    headers={'host':'rustfs:9000','x-amz-content-sha256':digest(body),'x-amz-date':stamp,**condition}
    names=';'.join(sorted(headers));canonical_headers=''.join(k+':'+headers[k].strip()+'\n' for k in sorted(headers))
    request='\n'.join((method,path,'',canonical_headers,names,digest(body)))
    day=stamp[:8];scope=day+'/us-east-1/s3/aws4_request'
    message='AWS4-HMAC-SHA256\n'+stamp+'\n'+scope+'\n'+digest(request.encode())
    key=('AWS4'+secret).encode()
    for part in (day,'us-east-1','s3','aws4_request'):key=hmac.new(key,part.encode(),hashlib.sha256).digest()
    signature=hmac.new(key,message.encode(),hashlib.sha256).hexdigest()
    return {**headers,'authorization':'AWS4-HMAC-SHA256 Credential='+access+'/'+scope+', SignedHeaders='+names+', Signature='+signature}

class S3:
    """Single fixed private endpoint; no proxies, redirect, HTTP retry or SDK."""
    def __init__(self, clock, *, wrong_ca=False, wrong_secret=False):
        self.clock=clock
        self.access=read_bounded('/run/secrets/access',130).decode('ascii').strip()
        self.secret=read_bounded('/run/secrets/secret',258).decode('ascii').strip()
        need(re.fullmatch('[A-Za-z0-9_-]{1,128}',self.access) and 1<=len(self.secret)<=256 and all(33<=ord(c)<127 for c in self.secret),'Credential extent')
        self.signing_secret='wrong-development-key' if wrong_secret else self.secret
        ca=read_bounded('/certs/wrong-ca.crt' if wrong_ca else '/certs/ca.crt',16384)
        self.context=ssl.create_default_context(cadata=ca.decode('ascii'))
        # Resolve the fixed private name at setup, not in every timed request.
        self.address=socket.gethostbyname('rustfs')
        ip=ipaddress.ip_address(self.address);need(ip.version==4 and ip.is_private and not ip.is_loopback and not ip.is_link_local,'Private RustFS address')
    def sensitive(self, raw):
        # Inspect bounded XML/JSON/URL encodings too; redaction cannot rely on
        # a literal substring when an error echoes escaped credential text.
        from urllib.parse import unquote
        candidates=[raw]
        if len(raw)<=META_CAP:
            try:candidates.append(unquote(raw.decode('utf8')).encode('utf8'))
            except UnicodeError:pass
            if raw.lstrip().startswith(b'<') and b'<!' not in raw.upper():
                try:
                    xml_bound(raw);root=ElementTree.fromstring(raw)
                    for node in root.iter():
                        candidates.extend(v.encode('utf8') for v in [node.text or '',node.tail or '',*node.attrib.values()])
                except (ElementTree.ParseError,Refused):return True
            if raw.lstrip().startswith((b'{',b'[')):
                try:
                    stack=[decode(raw)]
                    while stack:
                        value=stack.pop()
                        if type(value) is str:candidates.append(value.encode('utf8'))
                        elif type(value) is dict:stack.extend(value.keys());stack.extend(value.values())
                        elif type(value) is list:stack.extend(value)
                except (ValueError,TypeError):return True
        return any(value.encode() in candidate for candidate in candidates for value in (self.access,self.secret) if value)
    def exchange(self,method,path,body,condition,deadline):
        need(method in ('GET','PUT') and (path in ('/','/health/ready') or re.fullmatch('/hsearch-[a-z0-9-]{1,54}(?:/[a-z0-9/-]{1,160})?',path)),'Private request selector')
        need(len(body)<=BODY_CAP and set(condition)<= {'if-match','if-none-match'},'HTTP body/condition bound')
        need(not condition or len(condition)==1,'HTTP condition shape')
        for value in condition.values():need(type(value) is str and 1<=len(value)<=256 and all(32<=ord(c)<127 for c in value),'Opaque condition bound')
        remaining=min(10,(deadline-self.clock.now())/1e9);need(remaining>0,'HTTP deadline')
        headers=sign(method,path,body,self.access,self.signing_secret,datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ'),condition)
        headers.update({'content-length':str(len(body)),'connection':'close','accept-encoding':'identity'})
        wire=(method+' '+path+' HTTP/1.1\r\n'+''.join(k+': '+v+'\r\n' for k,v in headers.items())+'\r\n').encode('ascii')
        need(len(wire)<=4096,'Request header bound')
        sent=None;received=bytearray();status=None;etag=None
        try:
            with socket.socket(socket.AF_INET,socket.SOCK_STREAM) as rawsock:
                rawsock.settimeout(remaining);rawsock.connect((self.address,9000))
                with self.context.wrap_socket(rawsock,server_hostname='rustfs') as sock:
                    sock.sendall(wire+body);sent=len(body)
                    with sock.makefile('rb',buffering=0) as stream:
                        line=stream.readline(2049);need(len(line)<=2048 and re.fullmatch(rb'HTTP/1\.[01] [1-5][0-9][0-9] [^\r\n]*\r\n',line),'HTTP status framing')
                        status=int(line.split(b' ',2)[1]);headers={};extent=len(line)
                        for _ in range(33):
                            sock.settimeout(max(.001,min(10,(deadline-self.clock.now())/1e9)))
                            line=stream.readline(2049);extent+=len(line)
                            need(len(line)<=2048 and extent<=16384 and line.endswith(b'\r\n'),'HTTP header bound')
                            if line==b'\r\n':break
                            parts=line[:-2].split(b':',1);need(len(parts)==2,'HTTP header framing')
                            name=parts[0].decode('ascii').lower();value=parts[1].decode('ascii').strip()
                            need(re.fullmatch('[a-z0-9-]{1,64}',name) and name not in headers and len(value)<=1024,'HTTP header field')
                            headers[name]=value
                        else:raise Refused('HTTP header count')
                        need('transfer-encoding' not in headers and headers.get('content-encoding','identity')=='identity','Unsupported response framing')
                        size=headers.get('content-length','');need(re.fullmatch('[0-9]{1,6}',size),'HTTP body length')
                        size=int(size);need(size<=(BODY_CAP if status==200 else META_CAP),'HTTP response body bound')
                        etag=headers.get('etag');need(etag is None or 1<=len(etag)<=256 and all(32<=ord(c)<127 for c in etag),'Opaque response tag bound')
                        while len(received)<size:
                            need(self.clock.now()<deadline,'HTTP receive deadline')
                            sock.settimeout(max(.001,min(10,(deadline-self.clock.now())/1e9)))
                            part=stream.read(min(4096,size-len(received)));need(bool(part),'Incomplete HTTP body');received.extend(part)
            code=None if status==200 else error_code(bytes(received))
            return Reply(status,etag,bytes(received),code,sent)
        except ssl.SSLCertVerificationError:return Reply(None,None,bytes(received),'TLSCertificate',sent)
        except (OSError,ValueError,UnicodeError):return Reply(status,etag,bytes(received),'TransportOrFraming',sent)

class Client:
    def __init__(self,transport,budget,journal,clock,deadline):
        self.transport=transport;self.budget=budget;self.journal=journal;self.clock=clock;self.deadline=deadline
    def call(self,stage,lane,key,method,body=b'',condition=None,batch=None):
        need(stage in STAGES and type(lane) is list and len(lane)<=2,'Request classification')
        need(len(body)<=BODY_CAP,'Request payload bound');condition=condition or {}
        # Acquire before reserving an actual attempt. Waiting is not an HTTP send.
        with self.budget.gate:
            self.journal.reserve(12288)
            try:identity=self.budget.enter(self.deadline,len(body))
            except Exception:
                self.journal.complete(12288,'admission_refused',at_ns=self.clock.now(),reason='HTTPBudgetOrDeadline')
                raise
            start=self.clock.now()
            metadata={'request':identity,'stage':stage,'lane':lane,'batch':batch,'key':key,'method':method,
                      'condition':condition,'body_bytes':len(body),'body_sha256':digest(body),
                      'metadata_hex':body.hex() if stage not in ('slot','payload','verify-payload') and len(body)<=META_CAP else None,
                      'start_ns':start}
            self.journal.emit('request_started',**metadata)
            reply=None
            try:reply=self.transport.exchange(method,key,body,condition,min(self.deadline,self.budget.deadline))
            except Exception:reply=Reply(None,None,b'','TransportException',None)
            finally:self.budget.leave()
            self.budget.returned(len(reply.body),reply.sent)
            sensitive=getattr(self.transport,'sensitive',lambda _:False)
            redacted=sensitive(reply.body) or reply.etag is not None and sensitive(reply.etag.encode())
            result={'request':identity,'end_ns':self.clock.now(),'status':reply.status,'etag':None if redacted else reply.etag,
                    'error':'RedactedServiceError' if reply.error is not None and sensitive(reply.error.encode()) else reply.error,'request_body_sent_bytes':reply.sent,'response_bytes':len(reply.body),
                    'response_sha256':digest(reply.body),'response_metadata_hex':reply.body.hex() if not redacted and len(reply.body)<=META_CAP and stage!='verify-payload' else None,
                    'redacted':bool(redacted)}
            self.journal.complete(12288,'request_finished',**result)
            reply.record=result
            return reply

def ok(reply):
    need(reply.status==200 and reply.error is None and reply.etag is not None and not reply.record['redacted'],'Unknown or failed mutation/read')
    return reply

def case_identity(trace,group,wait,path):
    return {'workload':trace['workload'],'seed':trace['seed'],'grouping':group,'wait_us':wait,'path':path}

def execute_case(client,journal,clock,trace,group,wait,path,prefix):
    """Open-loop offered schedule, bounded pending records, one job per lane.

    Natural timer/byte plans are independent of HTTP completion. Overload poisons
    the comparison instead of silently regrouping a partial admitted schedule.
    Every offered identity still obtains exactly one terminal disposition.
    """
    events=MODEL['validate_trace'](trace);rows=MODEL['batches'](trace,group,wait)
    lanes=sorted({tuple(MODEL['lane'](e,group)) for e in events});need(len(lanes)<=16,'Lane bound')
    state={lane:{'root':None,'checkpoint':None,'slot':0,'pages':[],'active':False} for lane in lanes}
    payloads={e['id']:MODEL['payload'](e) for e in events};need(sum(map(len,payloads.values()))+4*2*BODY_CAP<=PAYLOAD_RESIDENT,'Resident payload reservation')
    event_map={e['id']:e for e in events};pending=set();terminal={};ready=[];futures={};lock=threading.RLock();failed=[]
    def lane_key(lane):return '-'.join(map(str,lane))
    def terminal_event(identity,status,reason,commit=None):
        with lock:
            need(identity not in terminal,'Duplicate terminal event');terminal[identity]=status;pending.discard(identity)
            journal.emit('event_finished',id=identity,status=status,reason=reason,finished_ns=clock.now(),commit_ns=commit)
    def put(stage,lane,key,body,condition,batch=None):
        return ok(client.call(stage,list(lane),key,'PUT',body,condition,batch))
    # Activation is explicit and outside the offered epoch, but fully metered.
    for lane in lanes:
        base=prefix+'/'+lane_key(lane)
        state[lane]['checkpoint']=put('activation',lane,base+'/checkpoint',canonical({'page':None,'through':0}),{'if-none-match':'*'}).etag
        if path=='root-cas':state[lane]['root']=put('activation',lane,base+'/root',canonical({'slot':0,'payload':None,'sha256':None}),{'if-none-match':'*'}).etag
    epoch=clock.now();journal.emit('case_started',case=case_identity(trace,group,wait,path),prefix=prefix,epoch_ns=epoch,planned_batches=rows)
    def perform(number,row):
        lane=tuple(row['lane']);s=state[lane];ids=row['ids'];base=prefix+'/'+lane_key(lane)
        body=b''.join(payloads[i] for i in ids);need(len(body)<=BODY_CAP,'Encoded batch bound')
        with lock:
            need(all(i in pending for i in ids),'Batch admission incomplete')
        slot=s['slot']+1;key=base+('/slots/' if path=='slots' else '/payloads/')+str(slot)
        journal.emit('batch_started',batch=number,lane=list(lane),slot=slot,ids=ids,planned_emit_us=row['emit_us'],started_ns=clock.now(),payload_bytes=len(body),payload_sha256=digest(body))
        try:
            receipt=put('slot' if path=='slots' else 'payload',lane,key,body,{'if-none-match':'*'},number)
            if path=='root-cas':
                prior=ok(client.call('root-read',list(lane),base+'/root','GET',batch=number))
                need(prior.etag==s['root'],'Root lineage mismatch')
                rootbody=canonical({'slot':slot,'payload':key,'sha256':digest(body)})
                receipt=put('root-update',lane,base+'/root',rootbody,{'if-match':prior.etag},number);s['root']=receipt.etag
            commit=receipt.record['end_ns'];s['slot']=slot
            observed=ok(client.call('verify-payload',list(lane),key,'GET',batch=number))
            need(observed.body==body,'Immutable payload mismatch')
            s['pages'].append({'slot':slot,'key':key,'sha256':digest(body)})
            if slot%16==0:
                page=base+'/pages/'+str(slot);pagebody=canonical({'lane':list(lane),'through':slot,'records':s['pages']})
                need(len(pagebody)<=META_CAP,'Checkpoint page bound')
                put('checkpoint-page',lane,page,pagebody,{'if-none-match':'*'},number)
                rootbody=canonical({'page':page,'through':slot,'sha256':digest(pagebody)})
                s['checkpoint']=put('checkpoint-root',lane,base+'/checkpoint',rootbody,{'if-match':s['checkpoint']},number).etag;s['pages']=[]
            for identity in ids:terminal_event(identity,'committed','receipt-and-read',commit)
            journal.emit('batch_finished',batch=number,finished_ns=clock.now(),commit_ns=commit)
        except Exception:
            with lock:failed.append('BatchUnresolved')
            for identity in ids:
                if identity not in terminal:terminal_event(identity,'unknown','batch-unresolved')
    actions=[]
    for i,e in enumerate(events):actions.append((e['at_us'],1,i,'offer',e))
    for i,r in enumerate(rows):actions.append((r['emit_us'],0 if r['trigger']=='timer' and wait else 2,i,'flush',r))
    actions.sort(key=lambda r:r[:3])
    with ThreadPoolExecutor(max_workers=4) as pool:
        def dispatch():
            for lane,future in list(futures.items()):
                if future.done():future.result();del futures[lane]
            for number,row in list(ready):
                lane=tuple(row['lane'])
                if lane not in futures and not failed:
                    ready.remove((number,row));futures[lane]=pool.submit(perform,number,row)
        for at,_priority,number,kind,item in actions:
            clock.until(epoch+at*1000);need(clock.now()<client.deadline,'Case deadline')
            dispatch()
            if kind=='offer':
                identity=item['id'];lane=tuple(MODEL['lane'](item,group))
                journal.emit('event_offered',id=identity,planned_ns=epoch+at*1000,offered_ns=clock.now())
                with lock:
                    count=sum(tuple(MODEL['lane'](event_map[i],group))==lane for i in pending)
                    if failed or count>=LANE_PENDING or len(pending)>=GLOBAL_PENDING:
                        failed.append('QueueBound');terminal_event(identity,'rejected','queue-or-poison')
                    else:
                        pending.add(identity);journal.emit('event_admitted',id=identity,enqueued_ns=clock.now(),pending=len(pending),lane_pending=count+1)
            else:ready.append((number,item))
            dispatch()
        while futures or ready:
            dispatch()
            if failed:
                for _number,row in ready:
                    for identity in row['ids']:
                        if identity in pending and identity not in terminal:terminal_event(identity,'rejected','poison-before-send')
                ready.clear()
            if futures:
                for future in list(futures.values()):future.result()
            need(clock.now()<client.deadline,'Case drain deadline')
    for identity in pending.copy():terminal_event(identity,'rejected','poison-before-send')
    need(set(terminal)==set(event_map),'Offered identity conservation')
    result={'offered':len(events),'committed':sum(v=='committed' for v in terminal.values()),'rejected':sum(v=='rejected' for v in terminal.values()),
            'unknown':sum(v=='unknown' for v in terminal.values()),'healthy':not failed,'actual_kms_calls':0,
            'modeled_data_wrapping_units':None if failed else sum(s['slot'] for s in state.values())}
    journal.emit('case_finished',finished_ns=clock.now(),**result)
    return result

def execute_faults(client,journal,clock,prefix):
    """Eight changed-body CAS rounds, then separate fixed ambiguity controls."""
    key=prefix+'/cas';initial=canonical({'round':-1,'generation':0,'contender':-1})
    first=ok(client.call('fault-create',[],key,'PUT',initial,{'if-none-match':'*'}));initial_tag=first.etag
    journal.emit('faults_started',prefix=prefix,initial_request=first.record['request'],started_ns=clock.now())
    previous=initial_tag;last_body=initial
    for number in range(8):
        read=ok(client.call('fault-read',[],key,'GET'));need(read.etag==previous and read.body==last_body,'CAS predecessor read')
        ready=[None]*4;barrier=threading.Barrier(5);replies=[None]*4
        def contend(contender):
            ready[contender]=clock.now();barrier.wait(timeout=10)
            body=canonical({'round':number,'generation':number+1,'contender':contender})
            replies[contender]=client.call('fault-cas',[],key,'PUT',body,{'if-match':read.etag},number)
        with ThreadPoolExecutor(max_workers=4) as pool:
            futures=[pool.submit(contend,i) for i in range(4)]
            # Main is the fifth participant; release follows all four arrivals.
            while any(value is None for value in ready):
                need(clock.now()<client.deadline,'CAS ready deadline');time.sleep(.0001)
            released=clock.now();barrier.wait(timeout=10)
            for future in futures:future.result()
        winners=[i for i,r in enumerate(replies) if r.status==200 and r.error is None]
        for r in replies:
            need(r.status==200 and r.error is None or (r.status,r.error) in ((412,'PreconditionFailed'),(409,'ConditionalRequestConflict')),'Unexpected CAS result')
        need(len(winners)==1,'CAS winner count')
        body=canonical({'round':number,'generation':number+1,'contender':winners[0]})
        resolved=ok(client.call('fault-read',[],key,'GET'));need(resolved.body==body and resolved.etag!=previous,'CAS winner resolution')
        journal.emit('cas_round',round=number,read_request=read.record['request'],ready_ns=ready,release_ns=released,
                     contenders=[r.record['request'] for r in replies],resolution_request=resolved.record['request'])
        previous=resolved.etag;last_body=body
    changed=canonical({'round':8,'generation':9,'contender':99})
    stale=client.call('fault-stale',[],key,'PUT',changed,{'if-match':initial_tag})
    need((stale.status,stale.error) in ((412,'PreconditionFailed'),(409,'ConditionalRequestConflict')),'Stale condition accepted')
    fresh=ok(client.call('fault-read',[],key,'GET'));need(fresh.body==last_body,'Conflict changed root')
    retried=ok(client.call('fault-retry',[],key,'PUT',changed,{'if-match':fresh.etag}))
    occupied=client.call('fault-occupied',[],key,'PUT',b'occupied-control',{'if-none-match':'*'})
    need((occupied.status,occupied.error) in ((412,'PreconditionFailed'),(409,'ConditionalRequestConflict')),'Occupied create accepted')
    latest=ok(client.call('fault-read',[],key,'GET'));need(latest.body==changed and latest.etag==retried.etag,'Retry/occupied resolution')
    loss_body=canonical({'request':prefix+'/lost-reply','identity':'unchanged'})
    loss=ok(client.call('fault-loss',[],prefix+'/loss','PUT',loss_body,{'if-none-match':'*'}))
    journal.emit('response_suppressed',request=loss.record['request'],caller_outcome='unknown',at_ns=clock.now(),boundary='after-observed-local-success')
    resolved=ok(client.call('fault-resolve',[],prefix+'/loss','GET'));need(resolved.body==loss_body and resolved.etag==loss.etag,'Lost response identity resolution')
    journal.emit('loss_resolved',original_request=loss.record['request'],resolution_request=resolved.record['request'],at_ns=clock.now())
    orphan_body=MODEL['payload'](MODEL['fixtures']()['traces'][0]['events'][0])
    orphan=ok(client.call('fault-orphan',[],prefix+'/orphan','PUT',orphan_body,{'if-none-match':'*'}))
    failed=client.call('fault-root-reject',[],key,'PUT',canonical({'orphan':prefix+'/orphan'}),{'if-match':initial_tag})
    need((failed.status,failed.error) in ((412,'PreconditionFailed'),(409,'ConditionalRequestConflict')),'Orphan root unexpectedly accepted')
    found=ok(client.call('fault-orphan',[],prefix+'/orphan','GET'));need(found.body==orphan_body,'Orphan payload missing')
    final=ok(client.call('fault-read',[],key,'GET'));need(final.body==changed and final.etag==retried.etag,'Failed root changed authority')
    journal.emit('faults_finished',finished_ns=clock.now(),rounds=8,contenders=32,winners=8,conflicts=24,
                 known_conflict_retry=1,unknown_resolved=1,orphans=1,actual_kms_calls=0,modeled_data_wrapping_units=2)


def runtime():
    need(sys.platform=='linux' and os.getuid()==10001 and os.getgid()==10001,'Owned Linux guest identity required')
    import platform
    caps={}
    for name in ('cpu.max','memory.max','memory.swap.max','pids.max'):
        with Path('/sys/fs/cgroup',name).open('rb') as f:raw=f.read(129)
        need(len(raw)<=128,'Kernel cap extent');caps[name]=raw.decode('ascii').strip()
    try:
        quota,period=map(int,caps['cpu.max'].split())
        need(quota>0 and period>0 and quota*1000==period*2000 and int(caps['memory.max'])==4*1024**3
             and caps['memory.swap.max']=='0' and caps['pids.max']=='256','Original object envelope required')
    except (ValueError,KeyError):raise Refused('Kernel cap shape') from None
    machine=platform.machine();need(machine in ('x86_64','aarch64'),'Native guest architecture')
    return {'uid':os.getuid(),'gid':os.getgid(),'architecture':machine,'caps':caps,'clock':'monotonic_ns',
            'scratch_bytes':18*1024**3,'scratch_proof':'requires independent outer aggregate-mount monitor'}


def fixed_inputs():
    with Path('/object-input/binding.json').open('rb') as f:raw=f.read(65537)
    inputs=decode(raw,65536);need(type(inputs) is dict and set(inputs)=={'schema','binding','bucket'} and inputs['schema']==1,'Fixed input schema')
    binding_check(inputs['binding']);binding=inputs['binding']
    need(binding['execution_kind']=='admitted-object','Synthetic execution cannot use fixed guest entry')
    need(type(inputs['bucket']) is str and re.fullmatch('hsearch-[a-z0-9-]{1,54}',inputs['bucket']),'Owned bucket selector')
    fixture=read_bounded(FIXTURE,MAX_FILE);need(len(fixture)<=MAX_FILE and digest(fixture)==binding['fixture_sha256'],'Fixture binding')
    need(decode(fixture,MAX_FILE)==MODEL['fixtures'](),'Fixture changed')
    need(digest(read_bounded(ROOT/'Cargo.lock',MAX_FILE))==binding['lock_sha256'],'Lock binding')
    need(digest(read_bounded(Path(__file__),MAX_FILE))==binding['driver_sha256'] and
         digest(read_bounded(Path(__file__).with_name('object_sample_verify.py'),MAX_FILE))==binding['oracle_sha256'],'Code binding')
    need(digest(canonical(plan()))==binding['plan_sha256'],'Plan binding')
    return inputs


def run_fixed():
    """Future integrator entry only; actual admission/export/monitor is external."""
    need(len(sys.argv)==1,'Fixed guest entry has no selectors')
    inputs=fixed_inputs();binding=inputs['binding'];actual=runtime()
    need(binding['target'].split('-')[0]==actual['architecture'],'Native binding mismatch')
    output=Path('/object-output');need(output.is_dir() and not output.is_symlink() and next(output.iterdir(),None) is None,'Fresh fixed output directory')
    clock=Clock();budget=Budget(clock);export=Export();journals=[];completed=0;healthy=0;failed=False
    def new(name):
        j=Journal(output/name,export);journals.append(j);return j
    journal=new('run.jsonl')
    journal.emit('run_started',schema=1,binding=binding,bucket=inputs['bucket'],runtime=actual,started_ns=clock.now(),plan=plan())
    def client_for(j,transport=None):return Client(transport or S3(clock),budget,j,clock,min(budget.deadline,clock.now()+CASE_NS))
    try:
        transport=S3(clock);client=client_for(journal,transport)
        wrong=client_for(journal,S3(clock,wrong_ca=True)).call('tls-negative',[],'/health/ready','GET')
        need(wrong.error=='TLSCertificate' and wrong.status is None,'Wrong CA control')
        bad=client_for(journal,S3(clock,wrong_secret=True)).call('credential-negative',[],'/','GET')
        need(bad.status==403 and bad.error in ('AccessDenied','SignatureDoesNotMatch','InvalidAccessKeyId'),'Credential control')
        bucket=client.call('bucket',[],'/'+inputs['bucket'],'PUT')
        need(bucket.status==200 and bucket.error is None,'Fresh owned bucket creation')
        for number,(trace,group,wait,path) in enumerate(cases()):
            case=new('case-'+str(number).zfill(3)+'.jsonl')
            case.emit('capture_started',schema=1,binding=binding,case_number=number,case=case_identity(trace,group,wait,path))
            c=client_for(case,transport);prefix='/'+inputs['bucket']+'/'+binding['run_id']+'/case-'+str(number).zfill(3)
            result=execute_case(c,case,clock,trace,group,wait,path,prefix)
            case.close();completed+=1;healthy+=int(result['healthy'] and result['unknown']==0)
            need(result['healthy'] and result['unknown']==0,'Object comparison incomplete/unhealthy')
        faults=new('faults.jsonl');faults.emit('capture_started',schema=1,binding=binding,case_number='faults')
        execute_faults(client_for(faults,transport),faults,clock,'/'+inputs['bucket']+'/'+binding['run_id']+'/faults');faults.close()
        import resource
        journal.emit('run_finished',finished_ns=clock.now(),completed_cases=completed,attempts=budget.attempts,
                     admission_body_bytes=budget.body_bytes,known_sent_body_bytes=budget.known_sent_body_bytes,
                     unknown_sent_attempts=budget.unknown_sends,peak_inflight=budget.peak,
                     peak_process_rss_bytes=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss*1024,
                     actual_kms_calls=0,scope='HTTP body counters exclude TLS/TCP framing; no production fencing/crypto/provider qualification')
        need(completed==144 and healthy==144,'Actual case matrix incomplete')
    except Exception:
        failed=True
        # No exception text or manufactured completion for partial requests.
        journal.emit('run_failed',finished_ns=clock.now(),completed_cases=completed,reason='ObjectWorkloadRefused')
    finally:
        for j in journals:
            if not j.file.closed:
                j.file.flush();os.fsync(j.file.fileno());j.file.close()
    descriptors=[]
    for j in journals:
        raw=j.path.read_bytes();need(len(raw)<=MAX_FILE,'Raw extent bound')
        descriptors.append({'file':j.path.name,'bytes':len(raw),'sha256':digest(raw)})
    index={'schema':1,'binding':binding,'files':descriptors,'total_bytes':sum(d['bytes'] for d in descriptors),
           'status':'failed' if failed else 'complete','completed_cases':completed,'healthy_cases':healthy,'binary_invocations':0,
           'scope':'Object API experiment; external normal execution/export proof remains mandatory'}
    raw=canonical(index)+b'\n';need(len(raw)<=MAX_FILE and export.used+len(raw)<=MAX_EXPORT,'Index/export bound')
    with (output/'index.json').open('xb') as f:f.write(raw);f.flush();os.fsync(f.fileno())
    print(json.dumps({'object_cases_executed':completed,'healthy_cases':healthy,'status':index['status'],
                      'index_sha256':digest(raw),'artifact_bytes':index['total_bytes'],
                      'scope':'Local object API observations only; independent oracle/controller qualification remains'}))
    need(not failed,'Object experiment incomplete; retain all raw extents')


if __name__=='__main__':
    try:run_fixed()
    except Exception:
        print('REFUSED: object experiment incomplete; preserve owned evidence',file=sys.stderr)
        raise SystemExit(1) from None
