"""Opaque failed-artifact protocol. Shape validation does not authenticate permission."""
import copy, hashlib, json, tarfile, time
from pathlib import Path
from . import _pack
from ._bytes import Refused, need
P=vars(_pack)
OP='failed-forensic-retention-v1'
CLAIM='m01-08-fb987723a42b'
OWNER='01a1ad0c-a57c-7462-9adc-44dda960885a'
PAIR=('hsearch-agent-fixture-3686832deb7c','sealed-export-qual3686832deb7c')
LIMITS=dict(archive_nodes=4096,archive_bytes=67108864,selected_nodes=84,selected_files=69,selected_bytes=224852,pack_bytes=2097152,index_bytes=1048576,raw_bytes=8192,public_bytes=16384,cpu_millis=2000,memory_bytes=4294967296,swap_bytes=0,pids=256,scratch_bytes=2147483648,duration_ms=1200000,audit_bytes=268435456,audit_rescans=32,range_bytes=65536,records=96,journal_events=72,record_bytes=131072,plan_slots=2,plan_bytes=1048576)

def enc(v):return json.dumps(v,sort_keys=True,separators=(',',':'),ensure_ascii=True,allow_nan=False).encode()


def digest(v):return hashlib.sha256(enc(v)).hexdigest()


def sha(v):need(type(v)is str and len(v)==64 and all(c in '0123456789abcdef' for c in v),'sha');return v


def integer(v,lo,hi):need(type(v)is int and lo<=v<=hi,'integer')


def keys(v,n):need(type(v)is dict and set(v)==set(n.split()),'schema')


def source(v):
    keys(v,'head tree fingerprint driver_fingerprint image_id clean');need(v['clean']is True,'source-dirty')
    for k in ['head','tree']:need(type(v[k])is str and len(v[k])==40 and all(c in '0123456789abcdef' for c in v[k]),'revision')
    for k in ['fingerprint','driver_fingerprint']:sha(v[k])
    need(type(v['image_id'])is str and v['image_id'].startswith('sha256:'),'image');sha(v['image_id'][7:]);return v


def audit_cost_bound(pack_bytes,outside_bytes):
    integer(pack_bytes,1,2097152);integer(outside_bytes,0,67108864-224852)
    # Complete construction, worker precharge, map/audit/retirement audits,
    # immediate pre-unlink hashes, and bounded outside preservation checks.
    bound=20*pack_bytes+12*224852+8*outside_bytes
    need(bound<=268435456,'audit-worst-case');return bound


def closure(entries):
    need(type(entries)is list and len(entries)==84,'selected-count')
    names=[];files=0;total=0
    for e in entries:
        keys(e,'path kind mode uid gid mtime_ns windows_attributes nlink bytes sha256')
        P['logical'](e['path']);need(e['path'].split('/')[0] in PAIR,'selected-scope')
        need(e['path'].casefold() not in names,'selected-alias');names.append(e['path'].casefold())
        need(e['kind']in ['file','directory'],'selected-type');integer(e['bytes'],0,P['MAX_FILE'])
        for k,lo,hi in [('mode',0,4095),('uid',0,2**32-1),('gid',0,2**32-1),('mtime_ns',-(2**63),2**63-1),('windows_attributes',0,2**32-1),('nlink',1,2**32-1)]:integer(e[k],lo,hi)
        need(not e['windows_attributes']&1024,'selected-reparse')
        if e['kind']=='file':files+=1;total+=e['bytes'];need(e['nlink']==1,'selected-links');sha(e['sha256'])
        else:need(e['bytes']==0 and e['sha256']is None,'directory')
        P['header'](e)
    need(files==69 and total==224852,'selected-denominator')
    need(all(any(e['path']==g and e['kind']=='directory' for e in entries) for g in PAIR),'selected-roots')
    by_name={e['path']:e for e in entries}
    for entry in entries:
        parts=entry['path'].split('/')
        for depth in range(1,len(parts)):
            parent='/'.join(parts[:depth])
            need(parent in by_name and by_name[parent]['kind']=='directory','selected-parent-closure')
    return dict(schema_version=1,kind='failed-forensic-closure-v1',claim=CLAIM,owner_session=OWNER,groups=list(PAIR),entries=copy.deepcopy(entries),original_cli_exit=1,guest_tests=601,original_source_head='fffbe1142717f45d538b2979021719db7f09d302',original_source_tree='86c6636f934d1648747a5d9d7849c2b776f48740',original_handoff_sha256='1b0d0e6bcd32f9919955714ad10e2fb4b5098bab0cbd60b7500de91ce1d4e0d8',original_cli_log_sha256='6d810a135da56b72f50db55d830cd8d88ed905ef71e573197e8aa7bef1990457',original_host_proof=False,cause='unconfirmed',qualification=False,semantic_export=False)


def manifest(closed,root,physical,binding,outside_sha):
    expected=closure(closed['entries'])
    need(closed==expected and enc(closed)==enc(expected),'closure');source(binding);sha(outside_sha)
    keys(physical,'entries bytes');integer(physical['entries'],84,4096);integer(physical['bytes'],224852,67108864)
    index=dict(schema_name='hsearch-retained-artifacts-v1',schema_version=1,claim=CLAIM,owner_session=OWNER,groups=sorted(PAIR),inventory_sha256s=[digest(closed)],closed_evidence_sha256=digest(closed),entries=closed['entries'])
    index_raw=enc(index);need(len(index_raw)<=P['MAX_INDEX'],'index-size');align=P['align']
    size=align(sum(512+align(e['bytes'],512) for e in closed['entries'])+512+align(len(index_raw),512)+1024,tarfile.RECORDSIZE)
    need(size<=LIMITS['pack_bytes'],'pack-size');need(physical['entries']+1<=4096 and physical['bytes']+size<=67108864,'capacity')
    plan=dict(schema_version=1,archive_root=str(root),claim=CLAIM,owner_session=OWNER,index=index,index_sha256=hashlib.sha256(index_raw).hexdigest(),pack_bytes=size,pack_sha256=None,original_physical_entries=physical['entries'],original_physical_bytes=physical['bytes'],original_logical_entries=physical['entries'],original_logical_bytes=physical['bytes'],selected_entries=84,selected_bytes=224852,staging_peak_entries=physical['entries']+1,staging_peak_bytes=physical['bytes']+size,final_physical_entries=physical['entries']-83,final_physical_bytes=physical['bytes']-224852+size,requires_maintenance_entry_exception=False,normal_entry_limit=4096,normal_byte_limit=67108864,activation='dormant-no-live-executor',authorization_created=False)
    P['validate_plan'](plan,digest(plan))
    return dict(schema_version=1,operation=OP,closure=copy.deepcopy(closed),source_binding=copy.deepcopy(binding),outside_sha256=outside_sha,limits=copy.deepcopy(LIMITS),payload_plan=plan,pack_name='failed-forensic-fff-'+digest(closed)[:12]+'.tar')


def validate(m):
    keys(m,'schema_version operation closure source_binding outside_sha256 limits payload_plan pack_name')
    need(type(m['schema_version'])is int and m['schema_version']==1 and m['operation']==OP,'operation')
    need(m['limits']==LIMITS and all(type(v)is int for v in m['limits'].values()),'limits')
    p=m['payload_plan'];rebuilt=manifest(m['closure'],p['archive_root'],dict(entries=p['original_physical_entries'],bytes=p['original_physical_bytes']),m['source_binding'],m['outside_sha256'])
    need(m==rebuilt and enc(m)==enc(rebuilt),'manifest-correspondence');need(len(enc(m))<=LIMITS['plan_bytes'],'manifest-size');return m


def request(m):validate(m);return dict(schema_version=1,operation=OP,manifest_sha256=digest(m))


def authorization(m,answer):
    # Pure shape, not authentication. Coordinator integration must obtain the actual human message.
    need(answer==f'Authorize forensic retention of ONLY the 84 FFF originals in {PAIR[0]} and {PAIR[1]}; one pack, one probe and one read-only worker; no retries or qualification approval.','human-scope')
    return dict(kind='failed-forensic-explicit-human-v1',manifest_sha256=digest(validate(m)),groups=list(PAIR),nodes=84,answer=answer,max_probe=1,max_worker=1,original_cli_exit=1,qualification=False)


def admission(m,auth,*,started_ms,deadline_ms):
    integer(started_ms,1,2**63-1);integer(deadline_ms,started_ms+1,started_ms+1200000)
    need(auth==authorization(m,auth['answer']),'authorization-binding')
    return dict(task_id='M01.17',component_version=1,manifest_sha256=digest(m),request_sha256=digest(request(m)),authorization_sha256=digest(auth),started_ms=started_ms,deadline_ms=deadline_ms,limits=copy.deepcopy(LIMITS))


class Meter:
    def __init__(self,a,clock=None):
        self.clock=clock or (lambda:time.time_ns()//1000000);self.start=self.clock();self.deadline=a['deadline_ms'];self.end=time.monotonic()+max(0,(self.deadline-self.start)/1000);self.used=0;self.rescans=0;self.denied=False
        need(a['started_ms']<=self.start<self.deadline<=a['started_ms']+1200000,'deadline')
    def check(self):
        if self.denied or self.clock()>=self.deadline or time.monotonic()>=self.end:
            self.denied=True
            raise Refused('expired-or-sticky')
    def charge(self,n,*,rescan=False):
        self.check();integer(n,0,268435456)
        if self.used+n>268435456 or self.rescans+int(rescan)>32:self.denied=True;raise Refused('audit-budget')
        self.used+=n;self.rescans+=int(rescan)


def audit_for(meter):
    meter.check();a=P['AuditBudget'](268435456,min(meter.deadline,meter.clock()+30000),max_rescans=32)
    original_check=a.check
    def check():meter.check();original_check()
    def charge(n,*,rescan=False):check();meter.charge(n,rescan=rescan);a.observed+=n;a.rescans+=int(rescan)
    a.check=check;a.charge=charge;return a


def composite(physical,m,present):
    validate(m);integer(present['entries'],0,84);integer(present['bytes'],0,224852)
    return dict(entries=physical['entries']+84-1-present['entries'],bytes=physical['bytes']+224852-m['payload_plan']['pack_bytes']-present['bytes'])


def semantic_reader(*_args,**_kwargs):raise Refused('opaque-failed-no-semantic-authority')
