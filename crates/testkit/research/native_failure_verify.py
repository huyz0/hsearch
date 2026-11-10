"""Independent persisted-file witnesses and protocol-order oracle for fixed histories."""
from collections import Counter
from pathlib import Path
import copy
import hashlib
import json
import runpy
import tempfile

W=runpy.run_path(str(Path(__file__).with_name('native_workload_verify.py')))
COMMIT=runpy.run_path(str(Path(__file__).with_name('native_commit_verify.py')))
V=W['V']
need,integer=V['need'],V['integer']
IDENTITIES={f'failure-{suffix}' for suffix in ('healthy','minority-offline','leader-unknown','held','suppressed','io-majority','four-voters','after-reopen')}
LABELS=('minority-offline','minority-recovered','unreplicated','leader-healed','held','suppressed','poisoned','reloaded',
        'learner-added','learner-caught-up','voter-added','four-voters','voter-removed','reopened','truncated')


def operations(value):
    need(type(value)is dict and len(value)<=128 and all(key==op['id'] for key,op in value.items()),'Persisted operation map identity')
    result=dict(V['operation'](op) for op in value.values())
    need(set(result)<=IDENTITIES,'Unexpected failure operation/new identity')
    return result


def configuration(raw):
    fields=V['protobuf'](raw,{1:(0,2),2:(0,2),3:(0,2),4:(0,2),5:0},(1,2,3,4))
    incoming=set(W['members'](fields,1));outgoing=set(W['members'](fields,3));learners=set(W['members'](fields,2))
    need(incoming and not incoming&learners and not outgoing and not W['members'](fields,4) and not V['scalar'](fields,5),
         'Fixed single-change configuration malformed or unexpectedly joint')
    return [incoming],learners


def raft_state(saved):
    need(set(saved)=={'schema_version','hard_state','entries','conf_state','snapshot','application'} and saved['schema_version']==2,'Raft scratch schema')
    hard=V['protobuf'](bytes(saved['hard_state']),{1:0,2:0,3:0})
    app=saved['application'];need(set(app)=={'applied','operations'},'Raft application shape')
    applied=integer(app['applied'],0,256,'persisted applied')
    durable={};expected_app={};previous=0;voters=[{1,2,3}];learners=set()
    if saved['snapshot']:
        snap=V['protobuf'](bytes(saved['snapshot']),{1:2,2:2})
        meta=V['protobuf'](V['scalar'](snap,2,b''),{1:2,2:0,3:0})
        snapshot=V['decode'](V['scalar'](snap,1,b''));previous=integer(V['scalar'](meta,2),1,256,'snapshot applied')
        need(snapshot['applied']==previous<=applied,'Raft snapshot prefix')
        expected_app=operations(snapshot['operations']);durable.update(expected_app)
        voters,learners=configuration(V['scalar'](meta,1,b''))
    need(type(saved['entries'])is list and len(saved['entries'])<=256,'Raft stored entry bound')
    for entry in saved['entries']:
        need(set(entry)=={'kind','term','index','data','context','sync_log'},'Raft stored entry fields')
        index=integer(entry['index'],1,256,'entry index');need(index==previous+1,'Raft persisted log hole')
        need(integer(entry['term'],1,2**64-1,'entry term')<=V['scalar'](hard,1),'Raft log term exceeds persisted vote')
        need(entry['context']==[] and entry['sync_log']is False,'Fixed history changed Entry flags')
        data=W['bytes_value'](entry['data'])
        if entry['kind']==0 and data:
            values=V['batch'](V['decode'](data));need(set(values)<=IDENTITIES and not set(values)&set(durable),'Duplicate/new operation in durable log')
            durable.update(values)
            if index<=applied:expected_app.update(values)
        elif entry['kind']==1:
            change=V['protobuf'](data,{1:0,2:0,3:0,4:2})
            target=integer(V['scalar'](change,3),1,4,'membership target');kind=integer(V['scalar'](change,2),0,2,'membership kind')
            if index<=applied:
                if kind==0:voters[0].add(target);learners.discard(target)
                elif kind==1:voters[0].discard(target);learners.discard(target)
                else:learners.add(target)
        else:need(entry['kind']==0 and not data,'Unexpected fixed history entry kind')
        previous=index
    need(applied<=previous and V['scalar'](hard,3)<=previous,'Raft durable committed/applied prefix')
    need(operations(app['operations'])==expected_app,'Raft application differs from durable applied prefix')
    need(configuration(bytes(saved['conf_state']))==(voters,learners),'Raft persisted membership differs from applied log/snapshot')
    return {'applied':expected_app,'durable':durable,'index':applied,'voters':voters,'learners':learners}


def log_id(value):
    need(type(value)is dict and set(value)=={'leader_id','index'} and set(value['leader_id'])=={'term','node_id'},'OpenRaft log id shape')
    integer(value['leader_id']['term'],0,2**64-1,'leader term');integer(value['leader_id']['node_id'],0,4,'leader id')
    return integer(value['index'],0,256,'OpenRaft index')


def membership(value):
    need(type(value)is dict and set(value)=={'configs','nodes'} and type(value['nodes'])is dict and
         all(key in {'1','2','3','4'} and node is None for key,node in value['nodes'].items()),'OpenRaft membership shape')
    configs=value['configs'];need(type(configs)is list and 1<=len(configs)<=2,'OpenRaft joint configuration count')
    sets=[]
    for config in configs:
        need(type(config)is list and 1<=len(config)<=4 and len(config)==len(set(config)) and
             all(type(node)is int and 1<=node<=4 for node in config),'OpenRaft voter set')
        sets.append(set(config))
    union=set.union(*sets);need(union<=set(map(int,value['nodes'])),'OpenRaft membership node missing')
    return sets,set(map(int,value['nodes']))-union


def openraft_state(log,machine):
    need(set(log)=={'vote','purged','committed','entries'} and log['purged']is None,'Fixed OpenRaft retained log shape')
    need(set(machine)=={'applied','snapshot'},'OpenRaft machine shape')
    app=machine['applied'];need(set(app)=={'last','membership','operations'},'OpenRaft applied tuple shape')
    applied=log_id(app['last']);stored=app['membership'];need(set(stored)=={'log_id','membership'},'OpenRaft stored membership shape')
    voters,learners=membership(stored['membership']);log_id(stored['log_id'])
    expected={};durable={};latest=None
    entries=log['entries'];need(type(entries)is dict and len(entries)<=256 and all(str(int(key))==key for key in entries),'OpenRaft log map keys')
    indices=sorted(map(int,entries));need(indices==list(range(len(indices))) and indices and applied<=indices[-1],'OpenRaft retained log prefix')
    for index in indices:
        entry=entries[str(index)];need(set(entry)=={'log_id','payload'} and log_id(entry['log_id'])==index,'OpenRaft stored Entry identity')
        payload=entry['payload']
        if payload=='Blank':continue
        need(type(payload)is dict and len(payload)==1,'OpenRaft payload enum')
        if 'Normal' in payload:
            values=V['batch'](payload['Normal']);need(set(values)<=IDENTITIES and not set(values)&set(durable),'Duplicate/new OpenRaft log operation')
            durable.update(values)
            if index<=applied:expected.update(values)
        else:
            need(set(payload)=={'Membership'},'OpenRaft payload kind');membership(payload['Membership'])
            if index<=applied:latest={'log_id':entry['log_id'],'membership':payload['Membership']}
    need(latest==stored and operations(app['operations'])==expected,'OpenRaft application/membership differs from committed log prefix')
    need(type(log['vote'])is dict and set(log['vote'])=={'leader_id','committed'} and type(log['vote']['committed'])is bool,'OpenRaft persisted vote')
    if log['committed']is not None:need(log_id(log['committed'])<=indices[-1],'OpenRaft persisted commit exceeds log')
    if machine['snapshot']is not None:
        snapshot=machine['snapshot'];need(set(snapshot)=={'meta','data'},'OpenRaft persisted snapshot shape')
        saved=V['decode'](bytes(snapshot['data']));meta=snapshot['meta']
        need(saved['last']==meta['last_log_id'] and saved['membership']==meta['last_membership'] and
             log_id(saved['last'])<=applied and operations(saved['operations']).items()<=expected.items(),'OpenRaft snapshot tuple/content mismatch')
    return {'applied':expected,'durable':durable,'index':applied,'voters':voters,'learners':learners}


def order_events(rows,candidate,files=None):
    """Reconcile actual file/sync transactions before callback/Ready/apply records."""
    need(files is not None, 'Ordering requires completed transaction contents')
    transactions=V['Transactions'](files)
    pending={};generation={};ready={};ready_generation={};app_generation={};append_generation={};callback_generation={};durable_index={};snapshots={}
    for row in rows:
        transactions.observe(row)
        kind=row['kind'];directory=row.get('directory')
        if kind=='file_write_attempt':
            cycle=pending.setdefault(directory,{'bytes':0,'file':False,'directory':False})
            need(not cycle['file'],'Write after file sync before atomic commit')
        elif kind=='file_write':pending[directory]['bytes']+=row['bytes']
        elif kind=='sync_complete':
            cycle=pending.get(directory);need(cycle and cycle['bytes']>0,'Sync without written state')
            target=row['target'];need(not cycle[target] and (target=='file' or cycle['file']),'Directory sync before file sync/duplicate')
            cycle[target]=True
        elif kind=='disk_persisted':
            cycle=pending.pop(directory,None)
            need(cycle and cycle['file'] and cycle['directory'] and cycle['bytes']==row['bytes'] and
                 row['file_sync']is True and row['directory_sync']is True,'False persisted state before actual complete file/directory sync')
            generation[directory]=generation.get(directory,0)+1
        elif kind=='disk_error':pending.pop(directory,None)
        elif kind=='ready_persisted':
            transactions.current(row,f"node-{row['node']}")
            node=row['node'];need(node not in ready and generation.get(f'node-{node}',0)>0,'Ready persistence missing file sync/prior advance')
            version=generation[f'node-{node}'];previous=ready_generation.get(node,1)
            atomic=row.get('atomic_application_indices',[]);need(type(atomic)is list and len(atomic)<=256 and
                all(type(index)is int and 0<=index<=4096 for index in atomic),'Ready atomic application indices')
            changed=bool(row['entries']) or row.get('hard_state',False) or row.get('snapshot',False) or bool(atomic)
            if changed:need(version>previous,'Ready update before new actual file sync')
            ready[node]={'version':version,'previous':previous};ready_generation[node]=version
            durable_index[node]=max(durable_index.get(node,0),max(row['entries'],default=0),snapshots.get(node,0) if row['snapshot'] else 0)
        elif kind=='advance_append':need(ready.pop(row['node'],False),'Advance before persisted Ready')
        elif kind=='step_received':need(row['node']not in ready,'Stepped inbound before previous Ready advanced')
        elif kind=='append_readable_and_persisted':
            transactions.current(row,f"node-{row['node']}-log")
            node=row['node'];version=generation.get(f'node-{node}-log',0)
            need(version>append_generation.get(node,1),'Append readable before new actual log sync')
            append_generation[node]=version
            durable_index[node]=max(durable_index.get(node,-1),max(row['indices'],default=-1))
        elif kind=='flush_callback_success':
            transactions.current(row,f"node-{row['node']}-log")
            node=row['node'];version=append_generation.get(node,0)
            need(version>callback_generation.get(node,0) and row['after_actual_sync']is True,'False flush callback before persisted append')
            callback_generation[node]=version
        elif kind=='applied':
            node=row['node'];directory=f'node-{node}'+('-machine' if candidate=='openraft' else '')
            transactions.current(row,directory)
            version=generation.get(directory,0)
            need(version>=2 and version>=app_generation.get(node,0),'Application before actual synced state')
            if candidate=='raft-rs' and node in ready:
                need(ready[node]['version']>ready[node]['previous'],'Atomic application before new actual file sync')
            app_generation[node]=version
        elif kind=='snapshot_persisted' and candidate=='raft-rs':
            need(generation.get(f"node-{row['node']}",0)>0,'Snapshot declared before persisted file')
            durable_index[row['node']]=max(durable_index.get(row['node'],0),row['index'])
        elif kind=='wire':
            need(files is not None,'Wire ordering requires actual retained bytes')
            location=row['wire'];raw=files['artifacts/'+location['file']][location['offset']:location['offset']+location['bytes']]
            source=row['source']
            if candidate=='raft-rs':
                fields=V['protobuf'](raw,{1:0,2:0,3:0,4:0,5:0,6:0,7:2,8:0,9:2,10:0,11:0,12:2,13:0,14:0,15:0,16:0},(7,))
                for encoded in fields.get(7,[]):
                    entry=V['protobuf'](encoded,{1:0,2:0,3:0,4:2,5:0,6:2})
                    need(V['scalar'](entry,3)<=durable_index.get(source,0),'Persistence-dependent message before persisted Entry')
                if row['message']=='MsgAppendResponse' and not V['scalar'](fields,10):
                    need(V['scalar'](fields,6)<=durable_index.get(source,0),'Successful append response before persisted Entry')
                if 9 in fields:
                    snapshot=V['protobuf'](V['scalar'](fields,9),{1:2,2:2})
                    meta=V['protobuf'](V['scalar'](snapshot,2,b''),{1:2,2:0,3:0});index=V['scalar'](meta,2)
                    need(index<=durable_index.get(source,0),'Snapshot sent before actual persistence')
                    if not row['dropped']:snapshots[row['target']]=index
            elif row['message']=='append_request':
                request=V['decode'](raw)
                need(all(entry['log_id']['index']<=durable_index.get(source,-1) for entry in request['entries']),
                     'OpenRaft append message before persisted Entry')
    need(not pending and not ready,'Unfinished persisted state/Ready at history end')
    transactions.finish()
    return COMMIT['verify'](rows,candidate,files)


def partition_phases(rows):
    """Require the actual minority edge and both leader links in their own phases."""
    phases=[];active=None
    for row in rows:
        if row['kind']=='partition_changed':
            need(type(row['isolated'])is bool and type(row['node'])is int,'Partition phase field types')
            if row['isolated']:
                need(active is None,'Overlapping partition phases')
                active={'node':row['node'],'start':row['sequence'],'drops':[]}
            else:
                need(active is not None and active['node']==row['node'],'Partition heal without matching isolation')
                active['end']=row['sequence'];phases.append(active);active=None
        elif row['kind']=='wire' and row['dropped'] and active is not None:
            active['drops'].append(row)
    need(active is None and [phase['node'] for phase in phases]==[3,1],'Missing/substituted partition phase')
    minority,leader=phases
    need(any({row['source'],row['target']}=={1,3} for row in minority['drops']),
         'Minority phase has no actual blocked channel observation')
    need({row['target'] for row in leader['drops'] if row['source']==1 and row['target']in {2,3}}=={2,3},
         'Leader partition did not actually block both follower links')
    return phases


def verify(root,probe,candidate):
    base=V['verify'](root,probe,candidate,history_kind='failure');files=V['load'](root,probe['files'])
    rows=[V['decode'](line) for name in sorted(files) if '/events-' in name for line in files[name].splitlines()]
    ordering=order_events(rows,candidate,files)
    witnesses={}
    for row in base['witnesses']:
        key=(row['label'],row['directory']);need(key not in witnesses,'Duplicate failure state witness');witnesses[key]=row['state']
    need({label for label,_ in witnesses}==set(LABELS),'Missing/unknown failure witness phase')
    def inspect(label,node):
        if candidate=='raft-rs':return raft_state(witnesses[(label,f'node-{node}')])
        return openraft_state(witnesses[(label,f'node-{node}-log')],witnesses[(label,f'node-{node}-machine')])
    states={label:{node:inspect(label,node) for node in (1,2,3,4) if (label,f'node-{node}'+('-log' if candidate=='openraft' else '')) in witnesses}
            for label in LABELS if label!='truncated'}
    for label,by_node in states.items():need(set(by_node)==({1,2,3,4} if label in {'learner-caught-up','voter-added','four-voters','voter-removed'} or (label=='learner-added' and candidate=='openraft') else {1,2,3}),'Failure witness voter inventory')
    def majority(label,identity,voters=(1,2,3)):
        by_node=states[label];need(identity in by_node[1]['applied'] and sum(identity in by_node[node]['durable'] for node in voters)>len(voters)//2,'Witness lacks durable applied majority')
    majority('minority-offline','failure-minority-offline');need('failure-minority-offline'not in states['minority-offline'][3]['applied'],'Offline node applied unavailable entry')
    need('failure-minority-offline'in states['minority-recovered'][3]['applied'],'Minority recovery missing')
    for label,identity in [('unreplicated','failure-leader-unknown'),('held','failure-held')]:
        need({node for node,st in states[label].items() if identity in st['durable']}=={1} and
             not any(identity in st['applied'] for st in states[label].values()),'Unreplicated/held operation falsely persisted/applied on majority')
    majority('leader-healed','failure-leader-unknown');majority('suppressed','failure-suppressed')
    majority('poisoned','failure-io-majority');need('failure-io-majority'not in states['poisoned'][3]['durable'],'Poisoned node falsely persisted new entry')
    need('failure-io-majority'in states['reloaded'][3]['applied'],'Explicit node reload failed catch-up')
    for label in ('learner-added','learner-caught-up'):
        need(states[label][1]['voters']==[{1,2,3}] and states[label][1]['learners']=={4},'Learner incorrectly counted as voter')
    need(set(states['learner-caught-up'][4]['applied'])==IDENTITIES-{'failure-four-voters','failure-after-reopen'},'Learner did not catch up real application')
    need(states['voter-added'][1]['voters']==[{1,2,3,4}] and not states['voter-added'][1]['learners'],'Voter promotion absent')
    majority('four-voters','failure-four-voters',(1,2,3,4))
    need(states['voter-removed'][1]['voters']==[{1,2,3}] and not states['voter-removed'][1]['learners'],'Voter removal absent')
    majority('reopened','failure-after-reopen')
    for node in (1,2,3):
        if candidate=='raft-rs':final=raft_state(V['framed_state'](files[f'node-{node}/state.bin']))
        else:final=openraft_state(V['framed_state'](files[f'node-{node}-log/state.bin']),V['framed_state'](files[f'node-{node}-machine/state.bin']))
        need(set(final['applied'])==IDENTITIES and final['voters']==[{1,2,3}] and not final['learners'],'Final reopened voter application/configuration differs')
    bad_directory='node-4'+('-machine' if candidate=='openraft' else '')
    bad=witnesses[('truncated',bad_directory)];need(bad==V['framed_state'](files[bad_directory+'/state.bin']),'Refused corrupt state was replaced or freshened')
    try:
        if candidate=='raft-rs':raft_state(bad)
        else:openraft_state(V['framed_state'](files['node-4-log/state.bin']),bad)
    except (ValueError,KeyError):pass
    else:raise ValueError('Actual persisted snapshot was not truncated/refused')
    kinds=Counter(row['kind'] for row in rows)
    for kind in ('failure_begin','failure_complete','minority_offline_verified','minority_recovered','unreplicated_barrier','held_majority_barrier',
                 'client_reply_suppressed','poisoned_node_refused','node_reloaded','learner_caught_up','graceful_reopen_verified','truncated_snapshot_refused'):
        need(kinds[kind]==1,'Missing/duplicate failure schedule event: '+kind)
    need([(r['node'],r['isolated']) for r in rows if r['kind']=='partition_changed']==[(3,True),(3,False),(1,True),(1,False)],'Partition schedule substituted')
    changes=[r for r in rows if r['kind']=='membership_api_proposed'];need([r['label'] for r in changes]==['learner-added','voter-added','voter-removed'],'Membership API schedule substituted')
    need([r['api'] for r in changes]==(['propose_conf_change']*3 if candidate=='raft-rs' else ['add_learner','change_membership','change_membership']),'Membership API missing')
    need(Counter(base['acknowledged'])==Counter(IDENTITIES) and set(base['operation_payloads'])==IDENTITIES,'Failure operation/ack identity conservation')
    resolved=[r for r in rows if r['kind']=='identity_resolved'];need(len(resolved)==2 and {r['id'] for r in resolved}=={'failure-leader-unknown','failure-suppressed'} and
        all(r['resubmitted']is False and r['outcome']=='published' for r in resolved),'Unknown outcome retried/new identity or unresolved')
    for identity,label in [('failure-leader-unknown','unreplicated_barrier'),('failure-held','held_majority_barrier')]:
        barrier=next(r for r in rows if r['kind']==label);need(barrier['applied']is False and barrier['successful_reply']is False,'False successful minority/held reply')
        need(not any(r['kind']=='applied' and identity in r['ids'] and r['sequence']<barrier['sequence'] for r in rows),'Applied uncommitted minority/held entry')
    suppressed=next(r for r in rows if r['kind']=='client_reply_suppressed');resolution=next(r for r in resolved if r['id']=='failure-suppressed')
    need(suppressed['outcome']=='unknown' and suppressed['sequence']<resolution['sequence'] and
         all(r['sequence']>resolution['sequence'] for r in rows if r['kind']=='client_ack' and r['id']=='failure-suppressed'),'Suppressed response falsely acknowledged before resolution')
    need(base['counts']['io_errors']>=2,'Missing actual injected IO observations')
    phases=partition_phases(rows);blocked_proposal_targets=set()
    for row in phases[1]['drops']:
        if row['source']!=1 or row['target']not in {2,3}:continue
        span=row['wire'];raw=files['artifacts/'+span['file']][span['offset']:span['offset']+span['bytes']]
        values=(V['raft_message'] if candidate=='raft-rs' else V['openraft_message'])(raw,row)
        if 'failure-leader-unknown'in values:blocked_proposal_targets.add(row['target'])
    need(blocked_proposal_targets=={2,3},'Partition did not block actual unreplicated proposal on both links')
    for row in rows:
        if row['kind']=='step_peer_not_found':need(row['source']==4 and row['source_absent_from_progress']is True,'Unexpected rejected Raft peer response')
    return {'native_failure_verification':'pass','candidate':candidate,'counts':base['counts'],'operations':len(IDENTITIES),'witnesses':len(witnesses),'ordering':ordering,
            'partition_observations':[{'node':phase['node'],'dropped_attempts':len(phase['drops']),'edges':[list(edge) for edge in sorted({(r['source'],r['target']) for r in phase['drops']})]} for phase in phases],
            'artifact_bytes':base['actual_artifact_bytes'],'scope':'Fixed local failure/membership/snapshot and IO ordering component; full sample/matrix/source-review/coverage remains'}


def negative_controls(root,probe,candidate,scratch):
    """Corrupt bounded copies of actual retained bytes; never modify the original."""
    original=V['load'](root,probe['files']);verify(root,probe,candidate)
    source_rows=[V['decode'](line) for name in sorted(original) if '/events-' in name for line in original[name].splitlines()]
    scratch=Path(scratch).resolve();need(scratch.is_dir() and not scratch.is_symlink(),'Negative-control scratch root')
    controls=('false-flush-or-advance','send-before-persist','uncommitted-apply','blind-new-identity','missing-response','forged-state-witness','insufficient-four-voter-quorum')
    results=[]
    for control in controls:
        rows=copy.deepcopy(source_rows)
        if control=='false-flush-or-advance':
            row=next(r for r in rows if r['kind']==('advance_append' if candidate=='raft-rs' else 'flush_callback_success'))
            rows.remove(row);rows.insert(1,row)
        elif control=='send-before-persist':
            row=next(r for r in rows if r['kind']=='wire' and r['message']==('MsgAppend' if candidate=='raft-rs' else 'append_request'))
            rows.remove(row);rows.insert(1,row)
        elif control=='uncommitted-apply':
            row=next(r for r in rows if r['kind']=='applied' and r['node']==1 and 'failure-leader-unknown'in r['ids'])
            rows.remove(row);rows.insert(next(i for i,r in enumerate(rows) if r['kind']=='unreplicated_barrier'),row)
        elif control=='missing-response':
            row=next(r for r in rows if r['kind']=='wire' and r['message']==('MsgAppendResponse' if candidate=='raft-rs' else 'append_response'))
            rows.remove(row)
        elif control=='insufficient-four-voter-quorum':
            row=next(r for r in rows if r['kind']=='client_ack' and r['id']=='failure-four-voters');row['persisted_nodes']=[1,2]
        output={name:raw for name,raw in original.items() if not name.startswith('artifacts/')}
        segments=[bytearray()]
        for row in rows:
            if row['kind']not in {'wire','state_witness','disk_persisted','disk_reopened'}:continue
            old=row['wire'];raw=original['artifacts/'+old['file']][old['offset']:old['offset']+old['bytes']]
            if control=='blind-new-identity':
                before=b'failure-suppressed';after=b'failure-new-'+b'x'*(len(before)-len(b'failure-new-'));raw=raw.replace(before,after)
            elif control=='forged-state-witness' and row['kind']=='state_witness' and row['label']=='unreplicated' and row['directory']==('node-1' if candidate=='raft-rs' else 'node-1-machine'):
                saved=V['framed_state'](raw);identity='failure-leader-unknown';prefix=f'id={identity};tenant=0;key=0;stream=0;'
                app=saved['application'] if candidate=='raft-rs' else saved['applied']
                app['operations'][identity]={'id':identity,'tenant':0,'key':0,'stream':0,'payload':prefix+'#'*(1024-len(prefix))}
                body=json.dumps(saved,separators=(',',':')).encode('utf-8');raw=b'HSRCHD01'+len(body).to_bytes(8,'little')+body
            need(len(raw)<=V['CAP'],'Controlled corrupted extent cap')
            if len(segments[-1])+len(raw)>V['CAP']:segments.append(bytearray())
            row['wire']={'file':f'wire-{len(segments)-1:03}.bin','offset':len(segments[-1]),'bytes':len(raw)};segments[-1].extend(raw)
        for index,row in enumerate(rows):row['sequence']=index
        events=b''.join(json.dumps(row,separators=(',',':')).encode('utf-8')+b'\n' for row in rows)
        need(len(events)<=V['CAP'] and all(len(line)<=4096 for line in events.splitlines()),'Controlled event extent cap')
        output['artifacts/events-000.ndjson']=events
        output.update({f'artifacts/wire-{i:03}.bin':bytes(raw) for i,raw in enumerate(segments)})
        mutated=copy.deepcopy(probe);mutated['report']['artifact_bytes']=len(events)+sum(map(len,segments))
        mutated['files']=[{'file':name,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest()} for name,raw in sorted(output.items())]
        need(len(output)<=256 and sum(map(len,output.values()))<=32*V['CAP'],'Controlled artifact aggregate cap')
        with tempfile.TemporaryDirectory(prefix='hsearch-failure-control-',dir=scratch) as directory:
            destination=Path(directory).resolve();need(destination.is_relative_to(scratch),'Controlled cleanup scope')
            for name,raw in output.items():
                path=destination/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(raw)
            try:verify(destination,mutated,candidate)
            except ValueError as error:results.append({'control':control,'result':'refused','reason':str(error)})
            else:raise ValueError('Unsafe actual-artifact counterexample accepted: '+control)
    need(V['load'](root,probe['files'])==original,'Original evidence changed during controls')
    return {'actual_artifact_negative_controls':'pass','candidate':candidate,'controls':results,'original_unchanged':True}
