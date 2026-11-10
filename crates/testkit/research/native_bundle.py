"""Bounded immutable-byte packing for complete public native research samples.

The caller supplies measured execution bindings. Diagnostic Cargo-test bundles
cannot become strict nextest qualification by omitting the execution kind.
"""
from pathlib import Path
import hashlib
import json
import os
import re
import runpy
import stat

V=runpy.run_path(str(Path(__file__).with_name('native_verify.py')))
need,integer=V['need'],V['integer'];CAP=1048576;TOTAL=32*CAP
PATH=re.compile(r'(artifacts/(events-[0-9]{3}\.ndjson|wire-[0-9]{3}\.bin)|node-[1-4](-(log|machine))?/state\.bin)')
HASH=re.compile('[a-f0-9]{64}')


def bindings(value):
    keys={'source_digest','source_head','lock_sha256','fixture_sha256','envelope_sha256','image','target','profile','candidate',
          'repetition','execution','nextest_run_id','protoc_sha256'}
    need(type(value)is dict and set(value)==keys,'Native bundle binding fields')
    for key in ('source_digest','lock_sha256','fixture_sha256','envelope_sha256','protoc_sha256'):
        need(type(value[key])is str and HASH.fullmatch(value[key])is not None,'Native binding digest: '+key)
    need(type(value['source_head'])is str and re.fullmatch('[a-f0-9]{40}',value['source_head'])is not None,'Native source head')
    need(type(value['image'])is str and re.fullmatch('sha256:[a-f0-9]{64}',value['image'])is not None,'Native image binding')
    need(all(type(value[key])is str for key in ('target','profile','candidate')) and
         value['target']in {'x86_64-unknown-linux-gnu','aarch64-unknown-linux-gnu'} and value['profile']in {'dev','release'} and
         value['candidate']in {'raft-rs','openraft'},'Native target/profile/candidate binding')
    if value['execution']=='strict-nextest':
        integer(value['repetition'],1,3,'strict repetition')
        need(type(value['nextest_run_id'])is str and re.fullmatch('[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}',value['nextest_run_id'])is not None,
             'Actual strict nextest UUID missing')
    else:
        need(value['execution']=='diagnostic-cargo-test' and type(value['repetition'])is int and value['repetition']==0 and value['nextest_run_id']is None,
             'Diagnostic execution cannot supply strict qualification')


def validate_execution(sample,binding):
    bindings(binding)
    if binding['execution']=='strict-nextest':
        runtime=sample.get('runtime')
        need(sample['sample_schema']==2 and type(runtime)is dict and
             set(runtime)=={'strict','binding','caps','uid','gid','architecture','scope'} and runtime['strict']is True,
             'Strict sample lacks actual nextest/kernel runtime receipt')
        need(runtime['binding']==binding and runtime['architecture']==binding['target'].split('-')[0],
             'Native child run/source/target binding differs')
        integer(runtime['uid'],10001,10001,'native UID');integer(runtime['gid'],10001,10001,'native GID')
        caps=runtime['caps'];need(type(caps)is dict and set(caps)=={'memory.max','memory.swap.max','cpu.max','pids.max'} and
             caps['memory.max']=='4294967296' and caps['memory.swap.max']=='0' and caps['pids.max']=='256' and
             type(caps['cpu.max'])is str and len(caps['cpu.max'])<=64 and re.fullmatch('[0-9]+ [0-9]+',caps['cpu.max'])is not None,
             'Actual native kernel memory/swap/PID/CPU cap fields differ')
        quota,period=map(int,caps['cpu.max'].split());integer(quota,1,2**64-1,'CPU quota');integer(period,1,2**64-1,'CPU period')
        need(quota==2*period,'Actual native kernel CPU cap differs')
    elif sample['sample_schema']==2:
        runtime=sample.get('runtime');need(type(runtime)is dict and runtime.get('strict')is False and runtime.get('nextest_run_id')is None,
            'Diagnostic Cargo test cannot claim strict/nextest execution')


def cases(sample):
    need(type(sample)is dict and type(sample['sample_schema'])is int and sample['sample_schema']in {1,2} and type(sample['candidate'])is str and
         type(sample['profile'])is str and sample['candidate']in {'raft-rs','openraft'} and sample['profile']in {'dev','release'},
         'Complete native sample identity')
    integer(sample['elapsed_us'],1,90000000,'whole candidate sample deadline')
    workloads=sample['workloads'];scales=sample['scales']
    need(type(workloads)is list and len(workloads)==18 and type(scales)is list and all(type(row['groups'])is int for row in scales) and [row['groups'] for row in scales]==[1,4,16],
         'Complete native sample workload/scale matrix')
    result=[(f'workload-{i:02}',row['raw']) for i,row in enumerate(workloads)]+[('failure',sample['failure'])]
    for scale in scales:
        count=scale['groups'];need(len(scale['raw_groups'])==count,'Complete native scale group count')
        result.extend((f'scale-{count:02}-{i:02}',report) for i,report in enumerate(scale['raw_groups']))
    need(len(result)==40 and len({report['directory'] for _,report in result})==40,'Native duplicate/missing physical context')
    return result


def read_file(path,cap=CAP):
    need(path.is_file() and not path.is_symlink(),'Native bundle missing/redirected file')
    before=path.stat();need(before.st_nlink==1 and 1<=before.st_size<=cap,'Native bundle file length/link bound')
    with path.open('rb') as stream:raw=stream.read(cap+1)
    after=path.stat();need(len(raw)==before.st_size and (before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns,before.st_ctime_ns)==
         (after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns,after.st_ctime_ns),'Native file changed while reading')
    return raw


def collect(sample,artifact_root,destination,binding):
    """Pack only known stopped sample contexts; never delete their source scratch."""
    bindings(binding);reports=cases(sample);validate_execution(sample,binding)
    need(sample['candidate']==binding['candidate'] and sample['profile']==binding['profile'],'Native sample/binding substitution')
    artifact_root=Path(artifact_root);destination=Path(destination)
    need(artifact_root.is_absolute() and artifact_root.is_dir() and not artifact_root.is_symlink() and artifact_root.resolve()==artifact_root,
         'Native scratch anchor redirected/missing')
    need(destination.is_absolute() and destination.parent.resolve()==artifact_root.parent and not destination.exists(),
         'Native bundle output must be a new owned sibling of scratch')
    index={'schema_version':1,'binding':binding,'sample':sample,'cases':[]};payloads=[bytearray()];decoded_total=count=0
    for label,report in reports:
        root=Path(report['directory'])
        need(root.is_absolute() and root.parent==artifact_root and root.is_dir() and not root.is_symlink() and root.resolve()==root and
             re.fullmatch(r'hsearch-durability-'+re.escape(sample['candidate'])+r'-[0-9]+-[0-9]+-[0-9]+',root.name)is not None,
             'Native context escaped owned sample scratch')
        entries=[]
        for directory,dirs,names in os.walk(root,followlinks=False):
            current=Path(directory);need(len(dirs)+len(names)<=256,'Native context directory entry bound')
            dirs.sort()
            for name in dirs:need(not (current/name).is_symlink() and (current/name).resolve().is_relative_to(root),'Native context directory redirect')
            for name in sorted(names):
                path=current/name;relative=path.relative_to(root).as_posix();need(PATH.fullmatch(relative)is not None,'Unexpected native scratch file')
                raw=read_file(path);decoded_total+=len(raw);count+=1
                need(decoded_total<=TOTAL and count<=1024,'Native decoded sample byte/file bound')
                if len(payloads[-1])+len(raw)>CAP:payloads.append(bytearray())
                need(len(payloads)<=255,'Native retained bundle file bound')
                blob=f'payload-{len(payloads)-1:03}.bin';offset=len(payloads[-1]);payloads[-1].extend(raw)
                entries.append({'file':relative,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),'blob':blob,'offset':offset})
        need(1<=len(entries)<=256,'Native empty/oversized context inventory');index['cases'].append({'label':label,'files':entries})
    encoded=(json.dumps(index,separators=(',',':'),ensure_ascii=False)+'\n').encode('utf-8')
    need(len(encoded)<=CAP and decoded_total+len(encoded)<=TOTAL,'Native retained bundle byte bound including index')
    destination.mkdir(mode=0o700);manifest=[]
    for name,raw in [('index.json',encoded),*((f'payload-{i:03}.bin',bytes(payload)) for i,payload in enumerate(payloads))]:
        with (destination/name).open('xb') as stream:stream.write(raw)
        manifest.append({'file':name,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest()})
    return {'native_bundle':'packed','files':manifest,'total_bytes':decoded_total+len(encoded),'decoded_bytes':decoded_total,
            'decoded_files':count,'binding':binding,'scope':'Immutable complete research bytes; execution kind and independent full qualification remain explicit'}


def load(root,receipt,expected_binding):
    bindings(expected_binding);need(receipt['binding']==expected_binding,'Native receipt/source execution binding differs')
    root=Path(root);need(root.is_dir() and not root.is_symlink(),'Native bundle root missing/redirected')
    manifest=receipt['files'];need(type(manifest)is list and 2<=len(manifest)<=256,'Native retained file count')
    blobs={};total=0
    for entry in manifest:
        need(type(entry)is dict and set(entry)=={'file','bytes','sha256'},'Native retained manifest fields')
        name=entry['file'];need(type(name)is str and re.fullmatch(r'index\.json|payload-[0-9]{3}\.bin',name)is not None and name not in blobs,'Native retained path/duplicate')
        integer(entry['bytes'],1,CAP,'retained file');need(type(entry['sha256'])is str and HASH.fullmatch(entry['sha256'])is not None,'Native retained hash')
        raw=read_file(root/name);need(len(raw)==entry['bytes'] and hashlib.sha256(raw).hexdigest()==entry['sha256'],'Native retained byte/hash mismatch')
        total+=len(raw);need(total<=TOTAL,'Native retained aggregate byte bound');blobs[name]=raw
    need({path.name for path in root.iterdir()}==set(blobs) and all(path.is_file() for path in root.iterdir()),'Native bundle full inventory mismatch')
    need(receipt['total_bytes']==total,'Native retained aggregate total differs')
    need('index.json'in blobs,'Native bundle index missing');index=V['decode'](blobs['index.json'])
    need(type(index)is dict and set(index)=={'schema_version','binding','sample','cases'} and type(index['schema_version'])is int and index['schema_version']==1 and index['binding']==expected_binding,
         'Native immutable index fields/binding')
    sample=index['sample'];reports=cases(sample);validate_execution(sample,expected_binding);need(sample['candidate']==expected_binding['candidate'] and sample['profile']==expected_binding['profile'],'Native indexed sample substitution')
    need(type(index['cases'])is list and [row['label'] for row in index['cases']]==[label for label,_ in reports],'Native case inventory incomplete/reordered')
    payload_names=sorted(name for name in blobs if name!='index.json')
    need(payload_names==[f'payload-{i:03}.bin' for i in range(len(payload_names))],'Native payload segment gap')
    cursors={name:0 for name in payload_names};decoded={};decoded_total=count=0
    for case in index['cases']:
        need(set(case)=={'label','files'} and type(case['files'])is list and 1<=len(case['files'])<=256,'Native decoded case inventory')
        files={}
        for entry in case['files']:
            need(type(entry)is dict and set(entry)=={'file','bytes','sha256','blob','offset'},'Native indexed extent fields')
            name=entry['file'];blob=entry['blob'];need(type(name)is str and PATH.fullmatch(name)is not None and name not in files and blob in cursors,'Native decoded path/duplicate/blob')
            size=integer(entry['bytes'],1,CAP,'decoded file');offset=integer(entry['offset'],0,CAP,'payload offset')
            need(offset==cursors[blob] and offset+size<=len(blobs[blob]),'Native immutable extent gap/overlap/truncation')
            raw=blobs[blob][offset:offset+size];cursors[blob]+=size
            need(type(entry['sha256'])is str and HASH.fullmatch(entry['sha256'])is not None and hashlib.sha256(raw).hexdigest()==entry['sha256'],'Native decoded extent hash mismatch')
            files[name]=raw;count+=1;decoded_total+=size;need(count<=1024 and decoded_total<=TOTAL,'Native decoded aggregate bound')
        decoded[case['label']]=files
    need(all(cursors[name]==len(blobs[name]) for name in cursors),'Native unaccounted immutable payload bytes')
    need(count==receipt['decoded_files'] and decoded_total==receipt['decoded_bytes'] and decoded_total+len(blobs['index.json'])==total,'Native decoded/retained count mismatch')
    return index,decoded


def _file_identity(path):
    info = path.lstat()
    need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and 1 <= info.st_size <= CAP,
         'Native checkpoint file type/link/size')
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)


def _sync_file(path, expected):
    """Sync one validated immutable file; no deletion or journal authority."""
    descriptor = os.open(path, os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0) | getattr(os, 'O_NONBLOCK', 0))
    try:
        before = os.fstat(descriptor)
        need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1 and
             (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) == expected,
             'Native sync file identity changed')
        os.fsync(descriptor)
        after = os.fstat(descriptor)
        need((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) ==
             (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns),
             'Native file changed during sync')
    finally:
        os.close(descriptor)


def _sync_directory(path, expected):
    need(os.name == 'posix', 'Native directory durability requires a qualified POSIX host')
    descriptor = os.open(path, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0) | getattr(os, 'O_NOFOLLOW', 0))
    try:
        info = os.fstat(descriptor)
        need(stat.S_ISDIR(info.st_mode) and (info.st_dev, info.st_ino) == expected,
             'Native sync directory identity changed')
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def seal_completed_bundle(root, receipt, expected_binding):
    """Prepare the content barrier for later trusted pre-disposal integration.

    This does not publish/commit a task journal, remove a worker, release a lease,
    or grant campaign acceptance. The shared controller must durably commit the
    resulting reference before disposal. Any failure leaves all inputs intact.
    """
    root = Path(root)
    need(root.is_absolute() and root.resolve() == root, 'Native checkpoint root redirected')
    for name, maximum in (('total_bytes', TOTAL), ('decoded_bytes', TOTAL), ('decoded_files', 1024)):
        integer(receipt[name], 1, maximum, 'checkpoint '+name)
    load(root, receipt, expected_binding)
    directory = root.lstat()
    directory_identity = (directory.st_dev, directory.st_ino)
    identities = {item['file']:_file_identity(root/item['file']) for item in receipt['files']}
    for item in receipt['files']:
        _sync_file(root/item['file'], identities[item['file']])
    _sync_directory(root, directory_identity)
    # Verify the same complete source/content binding after the durability
    # barrier. A sync of unrelated or changed bytes is not this checkpoint.
    load(root, receipt, expected_binding)
    directory = root.lstat()
    need(stat.S_ISDIR(directory.st_mode) and (directory.st_dev, directory.st_ino) == directory_identity and
         all(_file_identity(root/name) == identity for name, identity in identities.items()),
         'Native checkpoint objects changed across content barrier')
    committed_content = {'binding':dict(expected_binding), 'files':[dict(item) for item in receipt['files']],
        'total_bytes':receipt['total_bytes'], 'decoded_bytes':receipt['decoded_bytes'],
        'decoded_files':receipt['decoded_files']}
    raw = json.dumps(committed_content, sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode('ascii')
    need(len(raw) <= CAP, 'Native checkpoint descriptor bound')
    return {'checkpoint_schema':1, 'contents_sha256':hashlib.sha256(raw).hexdigest(), **committed_content,
        'content_barrier':'files-and-directory-synced', 'task_journal':'not-committed',
        'disposal_authorized':False, 'qualification':'not-established-by-content-sync'}
