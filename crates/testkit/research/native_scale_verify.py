"""Active-group payload, final-voter and aggregate/RSS observation checks."""
from pathlib import Path
import runpy

W=runpy.run_path(str(Path(__file__).with_name('native_workload_verify.py')))
F=runpy.run_path(str(Path(__file__).with_name('native_failure_verify.py')))
V=W['V'];need,integer=V['need'],V['integer']


def memory(value):
    need(type(value)is dict and set(value)=={'rss_bytes','high_water_bytes'},'Whole process RSS observation shape')
    rss=integer(value['rss_bytes'],1,4*1024**3,'whole process RSS')
    need(rss<=integer(value['high_water_bytes'],1,4*1024**3,'whole process high water'),'RSS exceeds process high water')


def verify_group(root,probe,candidate,count,ordinal):
    base=V['verify'](root,probe,candidate,history_kind='scale');files=V['load'](root,probe['files'])
    rows=[V['decode'](line) for name in sorted(files) if '/events-' in name for line in files[name].splitlines()]
    ordering=F['order_events'](rows,candidate,files)
    begin=next(r for r in rows if r['kind']=='scale_group_begin');end=next(r for r in rows if r['kind']=='scale_group_stopped')
    need(begin['groups']==count and begin['ordinal']==ordinal and begin['voters']==3 and begin['physical_group']is True and
         end['groups']==count and end['all_drained']is True,'Scale group ordinal/topology/drain binding')
    identity=f'scale-{count}-{ordinal}';prefix=f'id={identity};tenant=0;key=0;stream=0;'
    op={'id':identity,'tenant':0,'key':0,'stream':0,'payload':prefix+'#'*(1024-len(prefix))};expected=dict([V['operation'](op)])
    need(base['operation_payloads']==expected and base['acknowledged']==[identity] and base['counts']['dropped']==0 and base['counts']['io_errors']==0,
         'Scale actual live operation/ack/fault accounting differs')
    applied={}
    for row in rows:
        if row['kind']=='applied':applied[row['node']]=row['index']
    need(set(applied)=={1,2,3},'Scale voter application missing')
    W['final_states'](files,candidate,expected,{identity},applied)
    if candidate=='openraft':
        for node in (1,2,3):
            saved=V['framed_state'](files[f'node-{node}-machine/state.bin'])
            need(F['membership'](saved['applied']['membership']['membership'])==([{1,2,3}],set()),'Scale OpenRaft membership changed')
    return {'native_scale_group':'pass','groups':count,'ordinal':ordinal,'counts':base['counts'],'artifact_bytes':base['actual_artifact_bytes'],'ordering':ordering}


def verify_cases(sample,groups,candidate):
    need(sample['candidate']==candidate and type(sample['cases'])is list and [c['groups'] for c in sample['cases']]==[1,4,16],
         'Scale matrix duplicate/missing/substituted')
    results=[]
    for case in sample['cases']:
        count=case['groups'];need(case['voters_per_group']==3 and len(case['raw_groups'])==count,'Scale topology/report count')
        for field in ('baseline_memory','ready_memory','idle_memory'):memory(case[field])
        integer(case['idle_window_us'],100000,15000000,'100ms idle observation')
        need(case['configured_heartbeat_interval_us']==100000,'Scale heartbeat configuration substituted')
        if candidate=='openraft':
            need(case['native_tickers_enabled']is True and case['native_heartbeats_enabled']is True and
                 case['native_tick_emitter_interval_us']==150000,'Scale disabled/misreported OpenRaft ticker')
        else:need(case['application_ticks_per_voter']==1,'Scale application tick schedule changed')
        rows=[]
        for ordinal,report in enumerate(case['raw_groups']):
            root,probe=groups[(count,ordinal)];need(probe['report']==report,'Scale raw report substitution')
            rows.append(verify_group(root,probe,candidate,count,ordinal))
        sums={key:sum(row['counts'][key] for row in rows) for key in rows[0]['counts']}
        need(all(case['aggregate_counts'][key]==value for key,value in sums.items()),'Scale aggregate reset/omitted group')
        for key in sums:
            integer(case['idle_before'][key],0,sums[key],'before idle count')
            integer(case['idle_after'][key],case['idle_before'][key],sums[key],'after idle count')
        need(sums['attempts']<=4096 and sums['serialized_bytes']<=16*1024**2,'Scale aggregate transport budget exceeded')
        results.append({'native_scale_case':'pass','groups':count,'counts':sums,'groups_verified':rows,'idle_window_us':case['idle_window_us'],
            'idle_attempts':case['idle_after']['attempts']-case['idle_before']['attempts'],
            'baseline_memory':case['baseline_memory'],'ready_memory':case['ready_memory'],'idle_memory':case['idle_memory'],
            'scope':'Whole process sampled observations, including previous allocator/process state; native tick phases and drain differ. No exact per-group/fleet claim.'})
    sums={key:sum(row['counts'][key] for row in results) for key in results[0]['counts']}
    need(all(sample['aggregate_counts'][key]==value for key,value in sums.items()),'Scale sample aggregate reset')
    need(set(groups)=={(count,ordinal) for count in (1,4,16) for ordinal in range(count)},'Unknown scale group artifact')
    return {'native_scale_verification':'pass','candidate':candidate,'cases':results,'counts':sums}
