"""Independent object-result oracle. Never imports the workload executor.

verify(directory, binding, execution) consumes the fixed bounded export and an
INDEPENDENT normal-controller receipt. execution must bind index_sha256, bucket,
worker_cid, image_sha256, raw_worker_sha256, runtime and monitor. The caller owns
actual source/image/mount/network/CID/transfer authentication; worker self-report
alone is not provenance. Synthetic receipts are reported as synthetic only.

The oracle rebuilds public payloads, validates offered schedules through the
existing independent FIFO/deadline validator, derives object effects from raw
request/response records, and recomputes conservation, timing and accounting.
No driver winner count, final-state-only report or generated schedule is proof
that a case executed. Failure/partial exports never become a complete144 pass.
"""
from __future__ import annotations
import hashlib
import json
from pathlib import Path
import re
import runpy

ROOT=Path(__file__).resolve().parents[3]
MODEL=runpy.run_path(str(ROOT/'tools/durability_research.py'))
MAX_FILE=1048576
MAX_TOTAL=32*1048576
MAX_LINE=32768
PATHS=('slots','root-cas')
BINDING_FIELDS={'run_id','source_head','source_tree','source_sha256','lock_sha256','fixture_sha256','envelope_sha256',
                'image_sha256','topology_sha256','config_sha256','driver_sha256','oracle_sha256','plan_sha256','target','execution_kind'}

class Invalid(ValueError): pass

def need(ok,message):
    if not ok:raise Invalid(message)

def integer(v,lo,hi,label):
    need(type(v) is int and lo<=v<=hi,label);return v

def canon(v):return json.dumps(v,sort_keys=True,separators=(',',':'),ensure_ascii=True).encode('ascii')
def sha(raw):return hashlib.sha256(raw).hexdigest()

def decode(raw,cap=MAX_LINE):
    need(type(raw) is bytes and len(raw)<=cap,'Oracle extent bound')
    depth=0;inside=False;escape=False
    for c in raw:
        if inside:
            if escape:escape=False
            elif c==92:escape=True
            elif c==34:inside=False
        elif c==34:inside=True
        elif c in (91,123):depth+=1;need(depth<=16,'Oracle depth bound')
        elif c in (93,125):depth-=1;need(depth>=0,'Oracle framing')
    need(depth==0 and not inside,'Oracle framing')
    def pairs(items):
        need(len(items)<=256 and len(dict(items))==len(items),'Oracle duplicate/large map');return dict(items)
    def bad(_):raise Invalid('Oracle nonfinite number')
    try:return json.loads(raw,object_pairs_hook=pairs,parse_float=bad,parse_constant=bad)
    except (ValueError,UnicodeError,RecursionError):raise Invalid('Oracle JSON') from None

def read_file(path,cap=MAX_FILE):
    need(not path.is_symlink() and path.is_file(),'Oracle regular file')
    before=path.stat();need(before.st_nlink==1 and 0<before.st_size<=cap,'Oracle file bound/link')
    with path.open('rb') as f:raw=f.read(cap+1)
    after=path.stat();need(len(raw)==before.st_size and len(raw)<=cap and
        (before.st_ino,before.st_size,before.st_mtime_ns)==(after.st_ino,after.st_size,after.st_mtime_ns),'Oracle input changed')
    return raw

def rows(raw):
    need(len(raw)<=MAX_FILE and raw.endswith(b'\n'),'Raw extent/final framing')
    lines=raw.splitlines();need(1<=len(lines)<=4096,'Raw record count')
    result=[]
    for number,line in enumerate(lines):
        row=decode(line);need(type(row) is dict and row.get('seq')==number and type(row.get('seq')) is int and type(row.get('kind')) is str,'Raw sequence/schema')
        result.append(row)
    return result

def binding_check(b):
    need(type(b) is dict and set(b)==BINDING_FIELDS,'Oracle binding schema')
    for key in BINDING_FIELDS-{'target','execution_kind'}:
        n=32 if key=='run_id' else 40 if key in ('source_head','source_tree') else 64
        need(type(b[key]) is str and re.fullmatch('[0-9a-f]{'+str(n)+'}',b[key]),'Oracle binding identity')
    need(b['target'] in ('x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu') and b['execution_kind'] in ('synthetic','admitted-object'),'Oracle execution identity')

def payload(e):
    raw=('id='+e['id']+';tenant='+str(e['tenant'])+';key='+str(e['key'])+';stream='+str(e['stream'])+';').encode('ascii')
    need(len(raw)<=1024,'Public payload identity');raw+=b'#'*(1024-len(raw))
    need(sha(raw)==e['payload_sha256'],'Independent payload digest');return raw

def identity(t,g,w,p):return {'workload':t['workload'],'seed':t['seed'],'grouping':g,'wait_us':w,'path':p}
def matrix():return [(t,g,w,p) for t in MODEL['fixtures']()['traces'] for g in ('stream','compatible') for w in (0,10000,50000,250000) for p in PATHS]
def lane(e,g):return (e['stream'],) if g=='stream' else (e['tenant'],e['key'])
def only(records,kind):
    found=[r for r in records if r['kind']==kind];need(len(found)==1,'Missing/duplicate '+kind);return found[0]

def pairs(records,*,concurrent_same_lane=False):
    startkeys={'seq','kind','request','stage','lane','batch','key','method','condition','body_bytes','body_sha256','metadata_hex','start_ns'}
    endkeys={'seq','kind','request','end_ns','status','etag','error','request_body_sent_bytes','response_bytes','response_sha256','response_metadata_hex','redacted'}
    started={};finished={}
    for r in records:
        if r['kind']=='request_started':
            need(set(r)==startkeys,'Request schema/secret field')
            n=integer(r['request'],1,32768,'Request identity');need(n not in started,'Duplicate request')
            integer(r['start_ns'],0,2**63-1,'Request time');integer(r['body_bytes'],0,16384,'Request body size')
            need(type(r['body_sha256']) is str and re.fullmatch('[0-9a-f]{64}',r['body_sha256']),'Request digest')
            need(r['method'] in ('PUT','GET') and type(r['key']) is str and len(r['key'])<=256,'Request selector')
            need(type(r['lane']) is list and len(r['lane'])<=2 and all(type(x) is int and 0<=x<=15 for x in r['lane']),'Request lane')
            need(type(r['condition']) is dict and len(r['condition'])<=1 and set(r['condition'])<={'if-match','if-none-match'},'Request condition schema')
            for value in r['condition'].values():need(type(value) is str and 1<=len(value)<=256 and all(32<=ord(c)<127 for c in value),'Opaque condition')
            if r['metadata_hex'] is not None:
                body=hex_body(r['metadata_hex']);need(len(body)==r['body_bytes'] and sha(body)==r['body_sha256'],'Raw request metadata')
            started[n]=r
        if r['kind']=='request_finished':
            need(set(r)==endkeys,'Response schema/secret field')
            n=integer(r['request'],1,32768,'Response identity');need(n in started and n not in finished,'Missing/duplicate response')
            need(integer(r['end_ns'],0,2**63-1,'Response time')>started[n]['start_ns'],'Request interval')
            need(r['status'] is None or type(r['status']) is int and 100<=r['status']<=599,'HTTP status')
            need(r['etag'] is None or type(r['etag']) is str and 1<=len(r['etag'])<=256 and all(32<=ord(c)<127 for c in r['etag']),'Opaque response tag')
            need(r['error'] is None or type(r['error']) is str and re.fullmatch('[A-Za-z0-9]{1,64}',r['error']),'Public error code')
            need(r['request_body_sent_bytes'] is None or type(r['request_body_sent_bytes']) is int and 0<=r['request_body_sent_bytes']<=started[n]['body_bytes'],'Sent byte observation')
            integer(r['response_bytes'],0,16384,'Response body size')
            need(type(r['response_sha256']) is str and re.fullmatch('[0-9a-f]{64}',r['response_sha256']) and type(r['redacted']) is bool,'Response digest/redaction')
            if r['response_metadata_hex'] is not None:
                need(not r['redacted'],'Redacted body exposed');body=hex_body(r['response_metadata_hex'])
                need(len(body)==r['response_bytes'] and sha(body)==r['response_sha256'],'Raw response metadata')
            finished[n]=r
    need(set(started)==set(finished),'Incomplete actual request ledger')
    intervals=[];bylane={}
    for n,s in started.items():
        e=finished[n];intervals.extend(((s['start_ns'],1),(e['end_ns'],-1)));bylane.setdefault(tuple(s['lane']),[]).append((s['start_ns'],e['end_ns']))
    active=0;peak=0
    for _,delta in sorted(intervals):active+=delta;need(0<=active<=4,'Actual global in-flight cap');peak=max(active,peak)
    if not concurrent_same_lane:
        for intervals in bylane.values():
            intervals.sort();need(all(a[1]<=b[0] for a,b in zip(intervals,intervals[1:])),'Actual lane overlap')
    return started,finished,peak

def hex_body(value):
    need(type(value) is str and len(value)<=8192 and len(value)%2==0 and re.fullmatch('[0-9a-f]*',value),'Metadata hexadecimal extent')
    return bytes.fromhex(value)

def succeeded(s,e):
    need(e['status']==200 and e['error'] is None and not e['redacted'] and e['request_body_sent_bytes']==s['body_bytes'],'Unresolved/failed healthy request')
    need(e['etag'] is not None,'Missing opaque tag')

def expect_body(s,body):
    need(s['body_bytes']==len(body) and s['body_sha256']==sha(body),'Request body identity')
    if s['metadata_hex'] is not None:need(hex_body(s['metadata_hex'])==body,'Request raw content')

def expect_read(s,e,body,etag=None):
    succeeded(s,e);need(s['method']=='GET' and not s['condition'],'Read request semantics');expect_body(s,b'')
    need(e['response_bytes']==len(body) and e['response_sha256']==sha(body),'Read content/truncation')
    if etag is not None:need(e['etag']==etag,'Read version mismatch')
    if len(body)<=4096 and s['stage']!='verify-payload':need(e['response_metadata_hex'] is not None and hex_body(e['response_metadata_hex'])==body,'Missing raw metadata read')

def case_verify(raw,t,g,w,path,number,binding,bucket):
    binding_check(binding);need(path in PATHS,'Comparison selector');events=MODEL['validate_trace'](t);records=rows(raw)
    allowed={'capture_started','request_started','request_finished','case_started','event_offered','event_admitted','event_finished','batch_started','batch_finished','case_finished'}
    need(all(r['kind'] in allowed for r in records),'Unknown case record')
    cap=only(records,'capture_started');expected=identity(t,g,w,path)
    need(cap=={'seq':0,'kind':'capture_started','schema':1,'binding':binding,'case_number':number,'case':expected},'Case capture binding')
    beginning=only(records,'case_started');need(set(beginning)=={'seq','kind','case','prefix','epoch_ns','planned_batches'},'Case start schema')
    prefix='/'+bucket+'/'+binding['run_id']+'/case-'+str(number).zfill(3)
    need(beginning['case']==expected and beginning['prefix']==prefix,'Case path/identity')
    epoch=integer(beginning['epoch_ns'],0,2**63-1,'Case epoch');planned=beginning['planned_batches']
    MODEL['validate_batches'](t,g,w,16384,planned)
    s,e,peak=pairs(records);byid={v['id']:v for v in events};lanes=sorted({lane(v,g) for v in events})
    need(len(lanes)<=16 and len(events)*1024+4*32768<=512*1024,'Logical resident bound')
    batches={};batchend={};offered={};admitted={};terminal={};pending=set();maxpending=0;maxlane=0
    for r in records:
        kind=r['kind']
        if kind=='event_offered':
            need(set(r)=={'seq','kind','id','planned_ns','offered_ns'} and r['id'] in byid and r['id'] not in offered,'Offered identity')
            planned_ns=epoch+byid[r['id']]['at_us']*1000
            need(r['planned_ns']==planned_ns and integer(r['offered_ns'],0,2**63-1,'Offered time')>=planned_ns,'Changed/closed-loop offered trace')
            offered[r['id']]=r
        elif kind=='event_admitted':
            need(set(r)=={'seq','kind','id','enqueued_ns','pending','lane_pending'} and r['id'] in offered and r['id'] not in admitted,'Admission identity')
            need(integer(r['enqueued_ns'],0,2**63-1,'Enqueue time')>=offered[r['id']]['offered_ns'],'Enqueue order')
            pending.add(r['id']);count=sum(lane(byid[i],g)==lane(byid[r['id']],g) for i in pending)
            need(r['pending']==len(pending)<=512 and r['lane_pending']==count<=32,'Pending queue counter/bound')
            maxpending=max(maxpending,len(pending));maxlane=max(maxlane,count);admitted[r['id']]=r
        elif kind=='event_finished':
            need(set(r)=={'seq','kind','id','status','reason','finished_ns','commit_ns'} and r['id'] in admitted and r['id'] not in terminal,'Terminal identity')
            need(r['status']=='committed' and r['reason']=='receipt-and-read','Unhealthy disposition cannot qualify')
            need(admitted[r['id']]['enqueued_ns']<=integer(r['commit_ns'],0,2**63-1,'Commit time')<=integer(r['finished_ns'],0,2**63-1,'Terminal time'),'Commit/terminal order')
            need(r['id'] in pending,'Terminal pending conservation');pending.remove(r['id']);terminal[r['id']]=r
        elif kind=='batch_started':
            need(set(r)=={'seq','kind','batch','lane','slot','ids','planned_emit_us','started_ns','payload_bytes','payload_sha256'},'Batch record schema')
            n=integer(r['batch'],0,len(planned)-1,'Batch ordinal');need(n not in batches,'Duplicate batch')
            need(r['lane']==planned[n]['lane'] and r['ids']==planned[n]['ids'] and r['planned_emit_us']==planned[n]['emit_us'],'Actual versus planned batching')
            need(integer(r['started_ns'],0,2**63-1,'Batch start')>=epoch+r['planned_emit_us']*1000,'Early timer/final drain')
            need(all(i in admitted for i in r['ids']),'Unadmitted batch event')
            body=b''.join(payload(byid[i]) for i in r['ids']);need(len(body)<=16384 and r['payload_bytes']==len(body) and r['payload_sha256']==sha(body),'Batch payload')
            batches[n]=r
        elif kind=='batch_finished':
            need(set(r)=={'seq','kind','batch','finished_ns','commit_ns'} and r['batch'] in batches and r['batch'] not in batchend,'Batch completion')
            need(r['commit_ns']<=r['finished_ns'],'Batch completion time');batchend[r['batch']]=r
    need(set(offered)==set(admitted)==set(terminal)==set(byid) and not pending,'Dropped/duplicated offered event')
    need(set(batches)==set(batchend)==set(range(len(planned))),'Generated schedule is not executed matrix')
    specs=[];lane_state={x:{'slot':0,'page':[]} for x in lanes};commits={};sendtime={}
    def spec(stage,ln,key,method,body,condition,batch):specs.append((stage,list(ln),key,method,body,condition,batch))
    for ln in lanes:
        base=prefix+'/'+'-'.join(map(str,ln))
        spec('activation',ln,base+'/checkpoint','PUT',canon({'page':None,'through':0}),'absent',None)
        if path=='root-cas':spec('activation',ln,base+'/root','PUT',canon({'slot':0,'payload':None,'sha256':None}),'absent',None)
    for n,row in enumerate(planned):
        ln=tuple(row['lane']);ls=lane_state[ln];ls['slot']+=1;slot=ls['slot'];base=prefix+'/'+'-'.join(map(str,ln))
        need(batches[n]['slot']==slot,'Lane slot lineage');body=b''.join(payload(byid[i]) for i in row['ids'])
        key=base+('/slots/' if path=='slots' else '/payloads/')+str(slot)
        spec('slot' if path=='slots' else 'payload',ln,key,'PUT',body,'absent',n)
        if path=='root-cas':
            spec('root-read',ln,base+'/root','GET',b'',None,n)
            spec('root-update',ln,base+'/root','PUT',canon({'slot':slot,'payload':key,'sha256':sha(body)}),'match',n)
        spec('verify-payload',ln,key,'GET',b'',None,n)
        ls['page'].append({'slot':slot,'key':key,'sha256':sha(body)})
        if slot%16==0:
            page=base+'/pages/'+str(slot);content=canon({'lane':list(ln),'through':slot,'records':ls['page']})
            need(len(content)<=4096,'Checkpoint wire bound')
            spec('checkpoint-page',ln,page,'PUT',content,'absent',n)
            spec('checkpoint-root',ln,base+'/checkpoint','PUT',canon({'page':page,'through':slot,'sha256':sha(content)}),'match',n);ls['page']=[]
    need(len(specs)==len(s),'Omitted/unaccounted actual request')
    lookup={}
    for specrow in specs:
        key=(specrow[0],tuple(specrow[1]),specrow[2],specrow[6]);need(key not in lookup,'Oracle recipe duplicate');lookup[key]=specrow
    objects={};tags={}
    for request in sorted(s,key=lambda n:s[n]['start_ns']):
        start=s[request];end=e[request];key=(start['stage'],tuple(start['lane']),start['key'],start['batch'])
        need(key in lookup,'Wrong stage/key/lane/batch');stage,ln,name,method,body,condition,batch=lookup.pop(key)
        need(start['method']==method,'Wrong HTTP method');expect_body(start,body);succeeded(start,end)
        if batch is not None:
            need(start['start_ns']>=batches[batch]['started_ns'],'Request before batch start')
            sendtime[batch]=min(sendtime.get(batch,start['start_ns']),start['start_ns'])
        if method=='PUT':
            if condition=='absent':need(name not in objects and start['condition']=={'if-none-match':'*'},'Unconditional/occupied object write')
            else:need(name in objects and start['condition']=={'if-match':objects[name][1]},'Root CAS predecessor')
            need(end['response_bytes']==0 and end['response_sha256']==sha(b''),'Unexpected successful PUT body')
            need(end['etag'] not in tags.setdefault(name,set()),'Opaque token reuse/ABA');tags[name].add(end['etag'])
            objects[name]=(body,end['etag'])
            if stage in ('slot','root-update'):commits[batch]=end['end_ns']
        else:
            need(name in objects,'Read before creation');expect_read(start,end,*objects[name])
    need(not lookup,'Missing actual comparison operations')
    latencies=[];queuewait=[];lateness=[]
    for n,row in enumerate(planned):
        need(commits[n]==batchend[n]['commit_ns'],'False commit point')
        for i in row['ids']:
            need(terminal[i]['commit_ns']==commits[n] and sendtime[n]>=admitted[i]['enqueued_ns'],'Event receipt association')
            latencies.append(commits[n]-offered[i]['planned_ns']);queuewait.append(sendtime[n]-admitted[i]['enqueued_ns']);lateness.append(offered[i]['offered_ns']-offered[i]['planned_ns'])
    final=only(records,'case_finished')
    need(set(final)=={'seq','kind','finished_ns','offered','committed','rejected','unknown','healthy','actual_kms_calls','modeled_data_wrapping_units'},'Case summary schema')
    need(final['offered']==final['committed']==len(events) and final['rejected']==final['unknown']==final['actual_kms_calls']==0
         and final['healthy'] is True and final['modeled_data_wrapping_units']==len(planned),'Recounted case summary')
    need(epoch<=final['finished_ns']<=min(a['start_ns'] for a in s.values())+15_000_000_000 and all(x['finished_ns']<=final['finished_ns'] for x in terminal.values()),'Case deadline')
    return {'case':expected,'offered':len(events),'committed':len(events),'executed_batches':len(planned),'actual_requests':len(s),
            'request_ids':list(s),'known_sent_body_bytes':sum(e[n]['request_body_sent_bytes'] for n in s),
            'response_body_bytes':sum(x['response_bytes'] for x in e.values()),'peak_inflight':peak,'peak_pending':maxpending,'peak_lane_pending':maxlane,
            'enqueue_to_send_ns':queuewait,'planned_offer_to_commit_ns':latencies,'producer_lateness_ns':lateness,
            'actual_kms_calls':0,'modeled_data_wrapping_units':len(planned),'scope':'One actual-record oracle; source/controller/provider qualification remains external'}

def conflict(s,e):
    need((e['status'],e['error']) in ((412,'PreconditionFailed'),(409,'ConditionalRequestConflict')) and not e['redacted'],'Unexpected conditional error')
    need(e['request_body_sent_bytes']==s['body_bytes'] and e['response_metadata_hex'] is not None,'Conflict raw body missing')
    raw=hex_body(e['response_metadata_hex']);need(len(raw)<=4096 and b'<!' not in raw.upper(),'Conflict XML bound')
    from xml.etree import ElementTree
    need(len(re.findall(rb'<(?![/?!])',raw))<=64,'Conflict XML node admission')
    depth=0
    for tag in re.findall(rb'<([^<>]{1,2048})>',raw):
        if tag.startswith(b'?'):continue
        if tag.startswith(b'/'):depth-=1
        elif not tag.endswith(b'/'):depth+=1
        need(0<=depth<=8,'Conflict XML depth admission')
    need(depth==0,'Conflict XML framing')
    try:root=ElementTree.fromstring(raw)
    except ElementTree.ParseError:raise Invalid('Conflict XML') from None
    stack=[(root,0)];count=0
    while stack:
        node,depth=stack.pop();count+=1;need(count<=64 and depth<=8,'Conflict XML depth/nodes')
        stack.extend((child,depth+1) for child in node)
    need(root.findtext('Code')==e['error'],'Conflict code differs from raw response')


def faults_verify(raw,binding,bucket):
    records=rows(raw);need(all(r['kind'] in {'capture_started','request_started','request_finished','faults_started','cas_round','response_suppressed','loss_resolved','faults_finished'} for r in records),'Unknown fault record')
    cap=only(records,'capture_started');need(cap=={'seq':0,'kind':'capture_started','schema':1,'binding':binding,'case_number':'faults'},'Fault capture binding')
    prefix='/'+bucket+'/'+binding['run_id']+'/faults';rootkey=prefix+'/cas'
    first=only(records,'faults_started');need(first['prefix']==prefix,'Fault namespace')
    s,e,peak=pairs(records,concurrent_same_lane=True);used=set()
    def use(n,stage,method,key,body=None,condition=None):
        need(n in s and n not in used,'Fault request reference');used.add(n);a=s[n];b=e[n]
        need(a['stage']==stage and a['method']==method and a['key']==key and a['lane']==[],'Fault request selector')
        if body is not None:expect_body(a,body)
        if condition is not None:need(a['condition']==condition,'Fault condition association')
        return a,b
    initial=canon({'round':-1,'generation':0,'contender':-1})
    a,b=use(first['initial_request'],'fault-create','PUT',rootkey,initial,{'if-none-match':'*'});succeeded(a,b)
    tag=b['etag'];initialtag=tag;previous_end=b['end_ns'];lastbody=initial;seen={tag}
    rounds=[r for r in records if r['kind']=='cas_round'];need(len(rounds)==8,'CAS round matrix')
    for number,r in enumerate(rounds):
        need(set(r)=={'seq','kind','round','read_request','ready_ns','release_ns','contenders','resolution_request'} and r['round']==number,'CAS round schema/lineage')
        a,read=use(r['read_request'],'fault-read','GET',rootkey,b'',{});expect_read(a,read,lastbody,tag)
        need(a['start_ns']>previous_end,'CAS rounds overlap')
        need(type(r['ready_ns']) is list and len(r['ready_ns'])==4 and type(r['contenders']) is list and len(set(r['contenders']))==4,'CAS barrier participants')
        release=integer(r['release_ns'],0,2**63-1,'CAS release')
        need(all(read['end_ns']<=integer(x,0,2**63-1,'CAS ready')<=release for x in r['ready_ns']),'CAS ready/release')
        winners=[];starts=[];ends=[]
        for contender,n in enumerate(r['contenders']):
            body=canon({'round':number,'generation':number+1,'contender':contender})
            a,b=use(n,'fault-cas','PUT',rootkey,body,{'if-match':tag});need(a['batch']==number,'CAS round association')
            need(a['start_ns']>=release,'CAS send before barrier');starts.append(a['start_ns']);ends.append(b['end_ns'])
            if b['status']==200:
                succeeded(a,b);need(b['response_bytes']==0,'CAS receipt body');winners.append((body,b['etag']))
            else:conflict(a,b)
        need(max(starts)<min(ends),'CAS did not demonstrate common overlap')
        need(len(winners)==1,'CAS distinct winner count')
        a,resolved=use(r['resolution_request'],'fault-read','GET',rootkey,b'',{})
        need(a['start_ns']>=max(ends),'CAS premature resolution');expect_read(a,resolved,*winners[0])
        need(resolved['etag'] not in seen,'CAS unchanged/reused token');seen.add(resolved['etag'])
        tag=resolved['etag'];lastbody=winners[0][0];previous_end=resolved['end_ns']
    def selected(stage,method):
        found=[n for n in s if s[n]['stage']==stage and s[n]['method']==method and n not in used]
        need(len(found)==1,'Fault stage missing/duplicated');return found[0]
    changed=canon({'round':8,'generation':9,'contender':99})
    a,stale=use(selected('fault-stale','PUT'),'fault-stale','PUT',rootkey,changed,{'if-match':initialtag});conflict(a,stale)
    reads=sorted((n for n in s if s[n]['stage']=='fault-read' and n not in used),key=lambda n:s[n]['start_ns']);need(len(reads)==3,'Retry/final read inventory')
    a,fresh=use(reads[0],'fault-read','GET',rootkey,b'',{});expect_read(a,fresh,lastbody,tag);need(a['start_ns']>=stale['end_ns'],'Retry read order')
    a,retried=use(selected('fault-retry','PUT'),'fault-retry','PUT',rootkey,changed,{'if-match':fresh['etag']});succeeded(a,retried)
    need(a['start_ns']>=fresh['end_ns'] and retried['etag']!=tag,'Known conflict retry ordering/token')
    a,occupied=use(selected('fault-occupied','PUT'),'fault-occupied','PUT',rootkey,b'occupied-control',{'if-none-match':'*'});conflict(a,occupied)
    need(a['start_ns']>=retried['end_ns'],'Occupied control order')
    a,latest=use(reads[1],'fault-read','GET',rootkey,b'',{});expect_read(a,latest,changed,retried['etag']);need(a['start_ns']>=occupied['end_ns'],'Occupied control resolution')
    lostbody=canon({'request':prefix+'/lost-reply','identity':'unchanged'})
    loss_id=selected('fault-loss','PUT');a,loss=use(loss_id,'fault-loss','PUT',prefix+'/loss',lostbody,{'if-none-match':'*'});succeeded(a,loss)
    suppressed=only(records,'response_suppressed')
    need(set(suppressed)=={'seq','kind','request','caller_outcome','at_ns','boundary'} and suppressed['request']==loss_id
         and suppressed['caller_outcome']=='unknown' and suppressed['boundary']=='after-observed-local-success'
         and suppressed['at_ns']>=loss['end_ns'],'Unknown suppression boundary')
    resolution_id=selected('fault-resolve','GET');a,resolution=use(resolution_id,'fault-resolve','GET',prefix+'/loss',b'',{})
    expect_read(a,resolution,lostbody,loss['etag']);need(a['start_ns']>=suppressed['at_ns'],'Unknown resolution order')
    marker=only(records,'loss_resolved');need(marker['original_request']==loss_id and marker['resolution_request']==resolution_id and marker['at_ns']>=resolution['end_ns'],'Unknown exact-identity association')
    orphanbody=payload(MODEL['fixtures']()['traces'][0]['events'][0])
    a,orphan=use(selected('fault-orphan','PUT'),'fault-orphan','PUT',prefix+'/orphan',orphanbody,{'if-none-match':'*'});succeeded(a,orphan)
    a,failed=use(selected('fault-root-reject','PUT'),'fault-root-reject','PUT',rootkey,canon({'orphan':prefix+'/orphan'}),{'if-match':initialtag});conflict(a,failed)
    need(a['start_ns']>=orphan['end_ns'],'Orphan creation order')
    a,found=use(selected('fault-orphan','GET'),'fault-orphan','GET',prefix+'/orphan',b'',{});expect_read(a,found,orphanbody,orphan['etag']);need(a['start_ns']>=failed['end_ns'],'Orphan resolution order')
    a,finalread=use(reads[2],'fault-read','GET',rootkey,b'',{});expect_read(a,finalread,changed,retried['etag']);need(a['start_ns']>=found['end_ns'],'Final authority read order')
    need(used==set(s),'Hidden retry/extra fault request')
    final=only(records,'faults_finished')
    need(set(final)=={'seq','kind','finished_ns','rounds','contenders','winners','conflicts','known_conflict_retry','unknown_resolved','orphans','actual_kms_calls','modeled_data_wrapping_units'},'Fault summary schema')
    need((final['rounds'],final['contenders'],final['winners'],final['conflicts'],final['known_conflict_retry'],final['unknown_resolved'],final['orphans'],final['actual_kms_calls'])==(8,32,8,24,1,1,1,0) and final['modeled_data_wrapping_units']==2,'Fault recount/model path')
    need(final['finished_ns']>=finalread['end_ns'] and final['finished_ns']-min(a['start_ns'] for a in s.values())<=15_000_000_000,'Fault deadline')
    return {'cas_rounds':8,'cas_attempts':32,'cas_winners':8,'cas_conflicts':24,'known_conflict_retry':1,'suppressed_unknown_resolved':1,
            'orphan_count':1,'modeled_data_wrapping_units':2,'actual_requests':len(s),'request_ids':list(s),'peak_inflight':peak,
            'known_sent_body_bytes':sum(x['request_body_sent_bytes'] or 0 for x in e.values()),'response_body_bytes':sum(x['response_bytes'] for x in e.values())}


def expected_plan():
    return {'schema':1,'matrix':[[t['workload'],t['seed'],g,w,p] for t,g,w,p in matrix()],
            'payload_bytes':1024,'batch_bytes':16384,'lane_records':32,'lane_bytes':32768,'lanes':16,'pending_records':512,
            'resident_payload_bytes':512*1024,'requests':4,'request_per_lane':1,'checkpoint_batches':16,
            'case_ns':15_000_000_000,'command_ns':1_200_000_000_000,'attempts':32768,'body_bytes':64*1024*1024,
            'file_bytes':1048576,'export_bytes':32*1048576,
            'wrapping_model':{'data_object_units':1,'metadata_units':None,'actual_kms_calls':0},
            'faults':'8fresh-tag-rounds/4distinct-contenders;known-conflict-get+retry;stale;occupied;response-suppression;orphan'}


def verify(directory,binding,execution):
    """Full export oracle; execution is an independently authenticated input."""
    binding_check(binding);directory=Path(directory)
    need(directory.is_dir() and not directory.is_symlink(),'Export root')
    need(type(execution) is dict and set(execution)=={'kind','index_sha256','bucket','worker_cid','image_sha256','raw_worker_sha256','runtime','monitor','export_files'},'External execution/export schema')
    need(execution['kind']==binding['execution_kind'] and execution['image_sha256']==binding['image_sha256'],'External execution binding')
    for key in ('index_sha256','worker_cid','raw_worker_sha256'):
        need(type(execution[key]) is str and re.fullmatch('[0-9a-f]{64}',execution[key]),'External execution identity')
    bucket=execution['bucket'];need(type(bucket) is str and re.fullmatch('hsearch-[a-z0-9-]{1,54}',bucket),'Expected bucket')
    monitor=execution['monitor'];expected_monitor={'cpu_millis':2000,'memory_bytes':4*1024**3,'added_swap_bytes':0,'pids':256,
        'scratch_bytes':18*1024**3,'seconds':1200,'output_bytes':1048576,'exit_code':0,'truncated':False}
    need(type(monitor) is dict and set(monitor)==set(expected_monitor),'Original object monitor schema')
    for key,value in expected_monitor.items():
        if key=='output_bytes':integer(monitor[key],1,1048576,'Monitor diagnostic ceiling')
        else:need(type(monitor[key]) is type(value) and monitor[key]==value,'Original object monitor envelope/result')
    # The normal packet may enforce a stricter512KiB raw ceiling; this never
    # permits a larger diagnostic budget than the original1MiB object maximum.
    raw=read_file(directory/'index.json');need(sha(raw)==execution['index_sha256'],'Index transfer/source proof')
    index=decode(raw,MAX_FILE)
    need(type(index) is dict and set(index)=={'schema','binding','files','total_bytes','status','completed_cases','healthy_cases','binary_invocations','scope'}
         and index['schema']==1 and index['binding']==binding,'Index schema/binding')
    need(index['status']=='complete' and index['completed_cases']==index['healthy_cases']==144 and index['binary_invocations']==0,'Incomplete matrix is not144 executed')
    files=index['files'];need(type(files) is list and len(files)==146 and execution['export_files']==files,'Exact normal export inventory')
    expected={'run.jsonl','faults.jsonl'}|{'case-'+str(i).zfill(3)+'.jsonl' for i in range(144)}
    need({f['file'] for f in files}==expected and len(expected)==len(files),'Duplicate/missing raw case file')
    actual=set()
    for file in directory.iterdir():
        need(len(actual)<147 and file.name not in actual,'Export file count bound');actual.add(file.name)
    need(actual==expected|{'index.json'},'Unaccounted exported extent')
    total=0;decoded={}
    for descriptor in files:
        need(type(descriptor) is dict and set(descriptor)=={'file','bytes','sha256'},'Export descriptor schema')
        integer(descriptor['bytes'],1,MAX_FILE,'Raw exported size');total+=descriptor['bytes'];need(total+len(raw)<=MAX_TOTAL,'Aggregate export bound')
        body=read_file(directory/descriptor['file']);need(len(body)==descriptor['bytes'] and sha(body)==descriptor['sha256'],'Raw extent/hash')
        decoded[descriptor['file']]=body
    need(total==index['total_bytes'],'Export recount')
    fixture=read_file(ROOT/MODEL['FIXTURE']);need(sha(fixture)==binding['fixture_sha256'] and decode(fixture,MAX_FILE)==MODEL['fixtures'](),'Fixture provenance')
    need(sha(read_file(ROOT/'Cargo.lock'))==binding['lock_sha256'] and sha(read_file(Path(__file__)))==binding['oracle_sha256']
         and sha(read_file(Path(__file__).with_name('object_workload.py')))==binding['driver_sha256'],'Source code/lock provenance')
    run=rows(decoded['run.jsonl']);need(all(r['kind'] in {'run_started','request_started','request_finished','run_finished'} for r in run),'Run record schema')
    start=only(run,'run_started');finish=only(run,'run_finished')
    need(start['seq']==0 and start['schema']==1 and start['binding']==binding and start['bucket']==bucket and start['runtime']==execution['runtime'],'Raw run/source/runtime provenance')
    need(start['plan']==expected_plan() and sha(canon(start['plan']))==binding['plan_sha256'],'Independent fixed plan')
    runtime=start['runtime'];need(type(runtime) is dict and set(runtime)=={'uid','gid','architecture','caps','clock','scratch_bytes','scratch_proof'},'Runtime schema')
    caps=runtime['caps'];need(runtime['uid']==runtime['gid']==10001 and runtime['architecture']==binding['target'].split('-')[0]
         and runtime['scratch_bytes']==18*1024**3 and runtime['clock']=='monotonic_ns','Raw kernel identity')
    need(set(caps)=={'cpu.max','memory.max','memory.swap.max','pids.max'},'Raw cap schema')
    try:
        quota,period=map(int,caps['cpu.max'].split())
        need(quota>0 and period>0 and quota*1000==period*2000 and int(caps['memory.max'])==4*1024**3 and caps['memory.swap.max']=='0' and caps['pids.max']=='256','Raw kernel caps')
    except (ValueError,TypeError):raise Invalid('Raw kernel caps') from None
    s,e,peak=pairs(run);need(len(s)==3,'Transport preflight attempts')
    selected={v['stage']:n for n,v in s.items()};need(set(selected)=={'tls-negative','credential-negative','bucket'},'Transport preflight inventory')
    n=selected['tls-negative'];need(s[n]['key']=='/health/ready' and s[n]['method']=='GET' and e[n]['status'] is None and e[n]['error']=='TLSCertificate','Wrong CA was not refused')
    n=selected['credential-negative'];need(s[n]['key']=='/' and s[n]['method']=='GET' and e[n]['status']==403 and e[n]['error'] in ('AccessDenied','SignatureDoesNotMatch','InvalidAccessKeyId'),'Credential rejection not observed')
    n=selected['bucket'];need(s[n]['key']=='/'+bucket and s[n]['method']=='PUT' and not s[n]['condition'] and e[n]['status']==200 and e[n]['error'] is None,'Fresh owned bucket creation')
    case_results=[case_verify(decoded['case-'+str(i).zfill(3)+'.jsonl'],t,g,w,p,i,binding,bucket) for i,(t,g,w,p) in enumerate(matrix())]
    faults=faults_verify(decoded['faults.jsonl'],binding,bucket)
    all_ids=list(s)+faults['request_ids']+[n for case in case_results for n in case['request_ids']]
    need(len(all_ids)==len(set(all_ids))<=32768 and set(all_ids)==set(range(1,len(all_ids)+1)),'Physical attempt omission/reuse/counter')
    # Reopen raw pairs to derive whole-command counts, never trust summaries.
    sent=received=admission=unknown=0;all_intervals=[]
    for body in decoded.values():
        ss,ee,_=pairs(rows(body),concurrent_same_lane=True)
        for n,a in ss.items():
            b=ee[n];admission+=a['body_bytes']+b['response_bytes'];received+=b['response_bytes']
            if b['request_body_sent_bytes'] is None:unknown+=1
            else:sent+=b['request_body_sent_bytes']
            all_intervals.extend(((a['start_ns'],1),(b['end_ns'],-1)))
    active=0;actualpeak=0
    for _,delta in sorted(all_intervals):active+=delta;need(0<=active<=4,'Whole-command HTTP concurrency');actualpeak=max(active,actualpeak)
    need(finish['completed_cases']==144 and finish['attempts']==len(all_ids) and finish['admission_body_bytes']==admission<=64*1024*1024
         and finish['known_sent_body_bytes']==sent and finish['unknown_sent_attempts']==unknown and finish['peak_inflight']==actualpeak
         and finish['actual_kms_calls']==0,'Whole-command attempt/body/accounting mismatch')
    need(0<=finish['finished_ns']-start['started_ns']<=1_200_000_000_000,'Whole command deadline')
    integer(finish['peak_process_rss_bytes'],1,4*1024**3,'Observed process RSS')
    return {'object_observations':'verified','execution_kind':binding['execution_kind'],'actual_case_records':144,
            'offered':sum(c['offered'] for c in case_results),'actual_requests':len(all_ids),'known_sent_body_bytes':sent,
            'response_body_bytes':received,'unknown_sent_attempts':unknown,'faults':faults,'export_bytes':total,
            'directed_http_body_bytes':{'worker_to_rustfs_known':sent,'rustfs_to_worker':received,'uncertain_send_attempts':unknown},
            'scope':'Synthetic is HOST-only; admitted-object additionally requires independently authenticated normal controller/export proof. No provider/production/crypto/latency-SLO/fullM02.10 qualification.'}
