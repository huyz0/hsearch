"""Incoming bounded USTAR verification closure. No semantic Reader or extraction."""
import copy, hashlib, os, stat, tarfile, threading, time
from pathlib import Path, PurePosixPath
from . import _bytes
S = vars(_bytes)
need, keys, sha, digest, encoded = (S[k] for k in ('need','keys','sha','digest','encoded'))
MAX_ENTRIES=4096; MAX_BYTES=64*1024**2; MAX_LOGICAL=1024; MAX_FILE=8*1024**2; MAX_INDEX=1024**2
INDEX_NAME='__hsearch_retention_v1__/index.json'
_VIEW_TOKEN=object()

def bounded(value):
    nodes=0
    def walk(x,depth=0):
        nonlocal nodes
        nodes+=1;need(nodes<=50000 and depth<=24,'retention-json-bound')
        if isinstance(x,dict):
            for k,v in x.items():need(type(k) is str,'retention-json-key');walk(k,depth+1);walk(v,depth+1)
        elif isinstance(x,list):
            for v in x:walk(v,depth+1)
        elif isinstance(x,str):need(len(x)<=MAX_INDEX,'retention-string-bound')
        elif type(x) is int:need(-(2**63)<=x<2**63,'retention-integer-bound')
        else:need(x is None or type(x) is bool,'retention-json-type')
    walk(value);need(len(encoded(value))<=MAX_INDEX,'retention-json-bytes')


def logical(name):
    need(isinstance(name,str) and 0<len(name)<=512 and '\\' not in name and
         not any(ord(c)<32 for c in name),'retention-path')
    p=PurePosixPath(name)
    need(not p.is_absolute() and p.as_posix()==name and all(x not in ('.','..','') for x in name.split('/')),
         'retention-path')
    return name


def metadata(path):
    info=path.lstat();attrs=getattr(info,'st_file_attributes',0)
    need(not attrs & getattr(stat,'FILE_ATTRIBUTE_REPARSE_POINT',0x400) and not path.is_symlink() and
         path.resolve()==path.absolute(),'retention-redirect')
    directory=stat.S_ISDIR(info.st_mode)
    need(directory or stat.S_ISREG(info.st_mode) and info.st_nlink==1,'retention-file-type')
    return dict(kind='directory' if directory else 'file',mode=stat.S_IMODE(info.st_mode),uid=info.st_uid,gid=info.st_gid,
                mtime_ns=info.st_mtime_ns,windows_attributes=attrs,nlink=info.st_nlink,
                bytes=0 if directory else info.st_size)


def scan(root,groups=None,*,hash_files=False,limit=8192):
    """Bounded inspection, including directories; no links or extraction."""
    root=Path(root).absolute();need(root.resolve()==root and root.is_dir(),'retention-root')
    pending=[root/g for g in groups] if groups is not None else list_bounded(root,limit)
    out=[];total=0
    while pending:
        path=pending.pop();need(len(out)<limit,'retention-inspection-bound')
        item=metadata(path);name=logical(path.relative_to(root).as_posix())
        if item['kind']=='directory':pending.extend(list_bounded(path,limit-len(out)))
        else:
            total+=item['bytes'];need(total<=128*1024**2,'retention-inspection-bytes')
            if hash_files:
                checked=S['regular'](path,MAX_FILE,retain=False)
                need(checked['bytes']==item['bytes'] and metadata(path)==item,'retention-source-changed')
                item['sha256']=checked['sha256']
        if item['kind']=='directory' or not hash_files:item['sha256']=None
        out.append(dict(path=name,**item))
    return sorted(out,key=lambda x:x['path'])


def list_bounded(path,limit):
    out=[]
    for child in path.iterdir():
        need(len(out)<limit,'retention-inspection-bound');out.append(child)
    return out


def header(entry):
    info=tarfile.TarInfo(entry['path']);info.type=tarfile.DIRTYPE if entry['kind']=='directory' else tarfile.REGTYPE
    info.size=entry['bytes'];info.mode=entry['mode'];info.uid=entry['uid'];info.gid=entry['gid'];info.mtime=entry['mtime_ns']//10**9
    try:info.tobuf(format=tarfile.USTAR_FORMAT,encoding='utf-8',errors='strict')
    except (ValueError,UnicodeError,OverflowError):raise S['Refused']('retention-ustar-representation') from None
    return info


def align(value,bound):return (value+bound-1)//bound*bound


def validate_plan(value,pin):
    bounded(value)
    sha(pin);need(digest(value)==pin and type(value['schema_version']) is int and value['schema_version']==1 and value['activation']=='dormant-no-live-executor','retention-plan-pin')
    index=value['index'];keys(index,'schema_name schema_version claim owner_session groups inventory_sha256s closed_evidence_sha256 entries')
    need(index['schema_name']=='hsearch-retained-artifacts-v1' and type(index['schema_version']) is int and index['schema_version']==1 and
         index['claim']==value['claim'] and index['owner_session']==value['owner_session'],'retention-index-schema')
    need(isinstance(index['groups'],list) and 1<=len(index['groups'])<=3 and len(set(index['groups']))==len(index['groups']),'retention-index-groups')
    need(isinstance(index['entries'],list) and 1<=len(index['entries'])<=MAX_LOGICAL,'retention-index-count')
    names=set();bytes_=0
    for entry in index['entries']:
        keys(entry,'path kind mode uid gid mtime_ns windows_attributes nlink bytes sha256');logical(entry['path'])
        need(entry['path'] not in names and any(entry['path']==g or entry['path'].startswith(g+'/') for g in index['groups']),'retention-index-path')
        names.add(entry['path']);need(entry['kind'] in ('file','directory'),'retention-index-type')
        S['integer'](entry['bytes'],0,MAX_FILE);S['integer'](entry['mode'],0,0o7777)
        for k in ('uid','gid','windows_attributes'):S['integer'](entry[k],0,2**32-1)
        S['integer'](entry['mtime_ns'],-(2**63),2**63-1);S['integer'](entry['nlink'],1,2**32-1)
        if entry['kind']=='file':sha(entry['sha256']);need(entry['nlink']==1,'retention-index-link')
        else:need(entry['bytes']==0 and entry['sha256'] is None,'retention-directory-data')
        bytes_+=entry['bytes'];header(entry)
    need([e['path'] for e in index['entries']]==sorted(names),'retention-index-order')
    raw=encoded(index);computed=align(sum(512+align(e['bytes'],512) for e in index['entries'])+512+align(len(raw),512)+1024,tarfile.RECORDSIZE)
    need(len(raw)<=MAX_INDEX and computed==value['pack_bytes'] and value['selected_entries']==len(index['entries']) and value['selected_bytes']==bytes_,
         'retention-plan-accounting')
    need(value['staging_peak_entries']==value['original_physical_entries']+1 and
         value['staging_peak_bytes']==value['original_physical_bytes']+computed and
         value['final_physical_entries']==value['original_physical_entries']-len(index['entries'])+1 and
         value['final_physical_bytes']==value['original_physical_bytes']-bytes_+computed,'retention-projection-accounting')
    need(value['normal_entry_limit']==MAX_ENTRIES and value['normal_byte_limit']==MAX_BYTES and
         value['requires_maintenance_entry_exception']==(value['staging_peak_entries']>MAX_ENTRIES) and
         value['staging_peak_bytes']<=MAX_BYTES and value['pack_sha256'] is None,'retention-plan-limits')
    need(hashlib.sha256(encoded(value['index'])).hexdigest()==value['index_sha256'],'retention-index-pin')


class AuditBudget:
    """Fresh bounded read-only audit allowance, never a construction deadline."""
    def __init__(self,maximum_bytes,deadline_ms,max_rescans=24):
        S['integer'](maximum_bytes,1,256*1024**2);S['integer'](max_rescans,1,32)
        now=time.time_ns()//1000000;S['integer'](deadline_ms,now+1,now+30000)
        self.maximum=maximum_bytes;self.deadline=deadline_ms;self.end=time.monotonic()+(deadline_ms-now)/1000
        self.observed=0;self.rescans=0;self.max_rescans=max_rescans;self.failed=False
    def check(self):
        need(not self.failed,'retained-audit-sticky')
        if time.time_ns()//1000000>=self.deadline or time.monotonic()>=self.end:
            self.failed=True;raise S['Refused']('retained-audit-deadline')
    def charge(self,count,*,rescan=False):
        self.check();S['integer'](count,0,MAX_BYTES)
        self.observed+=count;self.rescans+=int(rescan)
        if self.observed>self.maximum or self.rescans>self.max_rescans:
            self.failed=True;raise S['Refused']('retained-audit-budget')


class RetainedMemberView:
    """Fixed verified member capability. No raw descriptor/outer offset API."""
    __slots__=('_session','_entry','_start','_evidence','_stamp')
    def __init__(self,session,entry,token):
        need(token is _VIEW_TOKEN and type(session) is VerifiedPack and session.audit is not None,'retained-member-construction')
        self._session=session;self._entry=copy.deepcopy(entry);self._start=session.offsets[entry['path']]
        self._stamp=self._path_stamp()
        self._evidence=dict(schema_version=1,plan_sha256=session.plan_pin,index_sha256=session.plan['index_sha256'],
            pack_sha256=session.sha256,name=entry['path'],bytes=entry['bytes'],sha256=entry['sha256'],metadata=copy.deepcopy(entry))
        need(0<=self._start<=self._start+entry['bytes']<=session.plan['pack_bytes'],'retained-member-extent')
    @property
    def evidence(self):return copy.deepcopy(self._evidence)
    @property
    def size(self):return self._entry['bytes']
    def _path_stamp(self):
        info=self._session.path.lstat()
        return (info.st_dev,info.st_ino,info.st_size,info.st_mtime_ns,info.st_ctime_ns,info.st_mode,info.st_uid,info.st_gid)
    def check(self):
        need(self._path_stamp()==self._stamp,'retention-pack-changed')
        self._session.audit.check();need(not self._session.stream.closed and self._session._identity()==self._session.identity,'retention-pack-changed')
    def close(self):self._session.close()
    def __enter__(self):return self
    def __exit__(self,*args):self.close()
    def read_exact_at(self,offset,length):
        S['integer'](offset,0,self.size);S['integer'](length,0,65536)
        need(offset+length<=self.size,'retained-member-range')
        with self._session._lock:
            self.check();self._session.stream.seek(self._start+offset);raw=self._session._read(length)
            need(len(raw)==length,'retained-member-truncated');self.check();return raw


class VerifiedPack:
    """Bounded explicit logical-path reader. No extraction/cache/Path illusion."""
    def __init__(self,path,value,pin,expected_pack_sha256=None,*,audit=None):
        validate_plan(value,pin);self.path=Path(path).absolute();self.plan=value;self.plan_pin=pin
        need(audit is None or type(audit) is AuditBudget,'retained-audit-type');self.audit=audit;self._lock=threading.RLock()
        if audit is not None:audit.charge(self.path.stat().st_size,rescan=True)
        checked=S['regular'](self.path,MAX_BYTES,expected_pack_sha256,retain=False)
        need(checked['bytes']==value['pack_bytes'],'retention-pack-size')
        self.sha256=checked['sha256'];self.stream=self.path.open('rb');self.offsets={}
        try:
            self.identity=self._identity()
            for entry in value['index']['entries']:
                info=self._header();expected=header(entry)
                need(info.name.rstrip('/')==entry['path'] and info.type==expected.type and info.size==expected.size and
                     (info.mode,info.uid,info.gid,info.mtime)==(expected.mode,expected.uid,expected.gid,expected.mtime),'retention-member')
                self.offsets[entry['path']]=self.stream.tell()
                hasher=hashlib.sha256();left=entry['bytes']
                while left:
                    raw=self._read(min(65536,left));need(raw,'retention-truncated');left-=len(raw);hasher.update(raw)
                if entry['kind']=='file':need(hasher.hexdigest()==entry['sha256'],'retention-member-hash')
                self._zeros(align(entry['bytes'],512)-entry['bytes'])
            info=self._header()
            need(info.name==INDEX_NAME and info.type==tarfile.REGTYPE and info.size==len(encoded(value['index'])) and info.size<=MAX_INDEX,'retention-index')
            raw=self._read(info.size)
            need(
                 raw==encoded(value['index']) and hashlib.sha256(raw).hexdigest()==value['index_sha256'],'retention-index')
            self._zeros(value['pack_bytes']-self.stream.tell())
            self.stream.seek(0);opened_hash=hashlib.sha256();left=value['pack_bytes']
            while left:
                raw=self._read(min(65536,left));need(raw,'retention-truncated');left-=len(raw);opened_hash.update(raw)
            need(opened_hash.hexdigest()==self.sha256,'retention-opened-pack-hash')
            need(self._identity()==self.identity,'retention-pack-changed')
        except BaseException:self.stream.close();raise

    def _read(self,count):
        if self.audit is not None:self.audit.charge(count)
        return self.stream.read(count)
    def member(self,name,expected_sha256,limit=MAX_FILE):
        logical(name);sha(expected_sha256);S['integer'](limit,0,MAX_FILE)
        need(self.audit is not None,'retained-audit-required');self.audit.check()
        entry=next((e for e in self.plan['index']['entries'] if e['path']==name),None)
        need(entry is not None and entry['kind']=='file' and entry['sha256']==expected_sha256 and entry['bytes']<=limit,'retained-member-reference')
        need(self._identity()==self.identity,'retention-pack-changed')
        return RetainedMemberView(self,entry,_VIEW_TOKEN)

    def _identity(self):
        a=os.fstat(self.stream.fileno());b=self.path.lstat()
        f=lambda x:(x.st_dev,x.st_ino,x.st_size,x.st_mtime_ns,x.st_nlink)
        need(f(a)==f(b) and stat.S_ISREG(a.st_mode) and a.st_nlink==1,'retention-pack-changed');return f(a)

    def _header(self):
        raw=self._read(512);need(len(raw)==512 and raw[257:263]==b'ustar\0','retention-ustar-header')
        try:info=tarfile.TarInfo.frombuf(raw,encoding='utf-8',errors='strict')
        except (ValueError,tarfile.TarError,UnicodeError):raise S['Refused']('retention-ustar-header') from None
        need(info.type in (tarfile.REGTYPE,tarfile.DIRTYPE) and not info.linkname and not info.pax_headers,'retention-member-type')
        return info

    def _zeros(self,count):
        need(0<=count<=MAX_BYTES,'retention-padding')
        while count:
            raw=self._read(min(65536,count));need(raw and not any(raw),'retention-padding');count-=len(raw)

    def read(self,name,expected_sha256,limit):
        logical(name);sha(expected_sha256);S['integer'](limit,0,MAX_FILE)
        need(self._identity()==self.identity,'retention-pack-changed')
        entry=next((e for e in self.plan['index']['entries'] if e['path']==name),None)
        need(entry is not None and entry['kind']=='file' and entry['sha256']==expected_sha256 and entry['bytes']<=limit,'retention-lookup')
        self.stream.seek(self.offsets[name]);raw=self._read(entry['bytes'])
        need(len(raw)==entry['bytes'] and hashlib.sha256(raw).hexdigest()==expected_sha256 and self._identity()==self.identity,'retention-read-hash')
        return raw

    def receipt(self):
        return dict(schema_version=1,plan_sha256=self.plan_pin,pack_sha256=self.sha256,index_sha256=self.plan['index_sha256'],
                    pack_bytes=self.plan['pack_bytes'],logical_entries=self.plan['selected_entries'],logical_bytes=self.plan['selected_bytes'],
                    physical_original_path_readability='not-asserted-explicit-logical-reader-only',
                    verification='structural-byte-proof-only',authorization_created=False)
    def close(self):self.stream.close()
    def __enter__(self):return self
    def __exit__(self,*_):self.close()
