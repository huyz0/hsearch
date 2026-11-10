"""Complete sample oracle; matrix/engine/CI acceptance is a separate controller gate."""
from contextlib import ExitStack
from pathlib import Path
import hashlib
import runpy
import tempfile

B=runpy.run_path(str(Path(__file__).with_name('native_bundle.py')))
W=runpy.run_path(str(Path(__file__).with_name('native_workload_verify.py')))
F=runpy.run_path(str(Path(__file__).with_name('native_failure_verify.py')))
S=runpy.run_path(str(Path(__file__).with_name('native_scale_verify.py')))
V=W['V'];need,integer=V['need'],V['integer']


def verify(root,receipt,binding,scratch):
    index,decoded=B['load'](root,receipt,binding);sample=index['sample'];candidate=sample['candidate']
    scratch=Path(scratch).resolve();need(scratch.is_dir() and not scratch.is_symlink(),'Native oracle scratch root')
    traces=W['MODEL']['fixtures']()['traces'];expected={(trace['workload'],trace['seed'],wait):(trace,wait) for trace in traces for wait in (50000,250000)}
    results=[];seen=set();probes={};all_counts=[];orderings=[]
    with ExitStack() as contexts:
        for label,report in B['cases'](sample):
            directory=Path(contexts.enter_context(tempfile.TemporaryDirectory(prefix='hsearch-sample-oracle-',dir=scratch))).resolve()
            need(directory.is_relative_to(scratch),'Native oracle cleanup scope')
            files=decoded[label]
            for name,raw in files.items():
                path=directory/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(raw)
            probe={'report':report,'files':[{'file':name,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest()} for name,raw in sorted(files.items())]}
            rows=[V['decode'](line) for name in sorted(files) if '/events-'in name for line in files[name].splitlines()]
            need(rows[0]['architecture']==binding['target'].split('-')[0],'Native raw architecture differs from execution binding')
            if sample['sample_schema']==2:
                need(rows[0].get('runtime')==sample['runtime'],'Native raw run/kernel/source receipt differs from complete sample')
            probes[label]=(directory,probe);all_counts.append(report['counts'])
        for ordinal,row in enumerate(sample['workloads']):
            case=row['case'];key=(case['workload'],case['seed'],case['wait_us']);need(key in expected and key not in seen,'Matched sample duplicate/missing case')
            seen.add(key);trace,wait=expected[key];directory,probe=probes[f'workload-{ordinal:02}'];probe['case']=case
            results.append(W['verify_case'](directory,probe,candidate,trace,wait))
            files=decoded[f'workload-{ordinal:02}'];rows=[V['decode'](line) for name in sorted(files) if '/events-'in name for line in files[name].splitlines()]
            orderings.append(F['order_events'](rows,candidate,files))
        need(seen==set(expected),'Matched sample incomplete')
        directory,probe=probes['failure'];failure=F['verify'](directory,probe,candidate)
        controls=F['negative_controls'](directory,probe,candidate,scratch)
        scale_groups={(count,ordinal):probes[f'scale-{count:02}-{ordinal:02}'] for count in (1,4,16) for ordinal in range(count)}
        scale_counts={key:sum(case['aggregate_counts'][key] for case in sample['scales']) for key in sample['scales'][0]['aggregate_counts']}
        scale=S['verify_cases']({'candidate':candidate,'cases':sample['scales'],'aggregate_counts':scale_counts},scale_groups,candidate)
        orderings.append(failure['ordering'])
        orderings.extend(group['ordering']for case in scale['cases']for group in case['groups_verified'])
        need(len(orderings)==40 and all(row['temporal_committed_application']=='pass'for row in orderings),'Complete temporal application context inventory')
    keys={'attempts','serialized_bytes','dropped','file_write_bytes','file_write_calls','sync_calls','io_errors','write_reserved_bytes','sync_reserved_calls'}
    need(set(sample['aggregate_counts'])==keys and all(set(row)==keys for row in all_counts),'Complete sample count schema')
    summed={key:sum(integer(row[key],0,2**63-1,'history count') for row in all_counts) for key in keys}
    need(sample['aggregate_counts']==summed,'Candidate aggregate reset/omitted history')
    bounds={'attempts':32768,'serialized_bytes':64*1048576,'file_write_bytes':512*1048576,'write_reserved_bytes':512*1048576,
            'sync_calls':65536,'sync_reserved_calls':65536,'file_write_calls':65536}
    for key,cap in bounds.items():integer(summed[key],0,cap,'whole sample '+key)
    need(summed['file_write_bytes']<=summed['write_reserved_bytes'] and summed['sync_calls']<=summed['sync_reserved_calls'],'Whole sample conservative reservation mismatch')
    integer(sample['elapsed_us'],1,90000000,'whole sample elapsed')
    return {'native_complete_sample':'pass','binding':binding,'workload_cases':len(results),'offered':sum(row['offered'] for row in results),
            'published':sum(row['published'] for row in results),'rejected':sum(row['rejected'] for row in results),'aggregate_counts':summed,
            'elapsed_us':sample['elapsed_us'],'failure':failure,'unsafe_controls':controls,'scales':scale,'retained_files':len(receipt['files']),
            'retained_bytes':receipt['total_bytes'],'decoded_files':receipt['decoded_files'],'decoded_bytes':receipt['decoded_bytes'],
            'temporal_application':{'status':'pass','contexts':len(orderings),'context_ids':list(probes),
                'certified_entries':sum(row['certified_entries']for row in orderings),
                'application_frames':sum(row['application_frames']for row in orderings),
                'committed_callbacks':sum(row['committed_callbacks']for row in orderings),
                'applied_events':sum(row['applied_events']for row in orderings)},
            'scope':'One complete independently checked candidate sample with explicit execution kind; native/profile/fresh repetition/engine/source coverage/final reviews/CI matrix acceptance remains separate'}
