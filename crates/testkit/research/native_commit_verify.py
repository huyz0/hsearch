"""Temporal certificates for the fixed three-voter/learner4 research profile.

Library callback, delivered serialized entry, durable quorum and synced
application-image witnesses are separate obligations. This is not a general
consensus implementation or a cryptographic execution attestation.
"""
from pathlib import Path
import hashlib
import json
import re
import runpy

V=runpy.run_path(str(Path(__file__).with_name('native_verify.py')))
need,integer=V['need'],V['integer']


def configurations(value):
    need(type(value)is list and 1<=len(value)<=2,'Temporal voter-set shape')
    result=[]
    for voters in value:
        need(type(voters)is list and 1<=len(voters)<=4,'Temporal voter-set size')
        members=frozenset(integer(v,1,4,'temporal voter')for v in voters)
        need(len(members)==len(voters),'Temporal duplicate voter')
        result.append(members)
    return tuple(result)


def open_membership(value):
    need(type(value) is dict and set(value)=={'configs','nodes'} and type(value['nodes']) is dict and
         len(value['nodes'])<=4 and all(key in {'1','2','3','4'} and node is None for key,node in value['nodes'].items()),
         'Completed OpenRaft membership shape')
    groups=configurations(value['configs'])
    need(set().union(*groups)<=set(map(int,value['nodes'])), 'OpenRaft voter missing from node map')
    return groups


def log_id(value):
    need(type(value)is dict and set(value)=={'leader_id','index'} and type(value['leader_id'])is dict and
         set(value['leader_id'])=={'term','node_id'},'Temporal OpenRaft log identity')
    return (integer(value['index'],0,4096,'temporal index'),
            integer(value['leader_id']['term'],0,2**63-1,'temporal term'),
            integer(value['leader_id']['node_id'],0,4,'temporal leader'))


def raft_id(value):
    need(type(value)is list and len(value)==2,'Temporal Raft entry identity')
    return (integer(value[0],1,4096,'temporal index'),integer(value[1],0,2**63-1,'temporal term'),0)


def raft_token(fields):
    # Compare every supported protobuf field, including context and sync_log.
    # Default omission and explicit defaults carry the same pinned Entry value.
    values = [V['scalar'](fields, n) for n in (1, 2, 3, 5)]
    values += [V['scalar'](fields, n, b'').hex() for n in (4, 6)]
    return hashlib.sha256(json.dumps(values, separators=(',', ':')).encode()).hexdigest()


def bytes_value(value):
    need(type(value) is list and len(value) <= V['CAP'] and
         all(type(v) is int and 0 <= v <= 255 for v in value), 'Stored transaction byte vector')
    return bytes(value)


def stored_data(value):
    need(type(value) is dict and len(value) == 1, 'Stored Entry data enum')
    if set(value) == {'Utf8'}:
        need(type(value['Utf8']) is str and len(value['Utf8']) <= V['CAP'], 'Stored UTF8 bounds')
        raw = value['Utf8'].encode('utf8')
        need(len(raw) <= V['CAP'], 'Stored UTF8 byte bound')
        return raw
    need(set(value) == {'Binary'}, 'Stored Entry data variant')
    return bytes_value(value['Binary'])


def conf_sets(raw):
    fields = V['protobuf'](raw, {1:(0,2),2:(0,2),3:(0,2),4:(0,2),5:0}, (1,2,3,4))
    groups = {}
    for number in (1,2,3,4):
        values = []
        for item in fields.get(number, []):
            if type(item) is int:
                values.append(integer(item, 1, 4, 'configuration node'))
            else:
                offset = 0
                while offset < len(item):
                    node, offset = V['varint'](item, offset)
                    values.append(integer(node, 1, 4, 'configuration node'))
                    need(len(values) <= 4, 'Configuration count')
        need(len(values) <= 4 and len(values) == len(set(values)), 'Configuration duplicate/count')
        groups[number] = values
    need(not set(groups[1]) & set(groups[2]) and V['scalar'](fields,5) in (0,1), 'Configuration role/auto-leave')
    if not any(groups.values()):return ()
    return configurations([groups[1]] + ([groups[3]] if groups[3] else []))


class History:
    def __init__(self,rows,candidate,files):
        need(candidate in {'raft-rs','openraft'},'Temporal candidate')
        self.candidate=candidate;self.definitions={};self.wires={};self.origin={};self.first_wire={}
        self.received=set();self.received_snapshots={};self.snapshots={};self.callbacks={n:{}for n in range(1,5)}
        self.entries={n:{}for n in range(1,5)};self.base={n:(0 if candidate=='raft-rs' else -1)for n in range(1,5)}
        self.tail=dict(self.base);self.frames={n:(self.base[n],frozenset())for n in range(1,5)}
        self.certificates={};self.by_index={};self.prefix=0 if candidate=='raft-rs'else -1
        self.voters={-1:(frozenset({1,2,3}),)};self.operations={};self.prefix_ids={self.prefix:frozenset()}
        self.applied=0;self.application_frames=0;self.callback_count=0
        self.transactions = V['Transactions'](files)
        self.entry_tokens = {n:{} for n in range(1,5)}
        self.images = {}
        self.quarantined = set()
        self.quarantine_observed = set()
        self.open_memberships = {}
        chunks={}
        for row in rows:
            if row['kind']!='wire':continue
            location=row['wire'];raw=files['artifacts/'+location['file']][location['offset']:location['offset']+location['bytes']]
            edge=integer(row['edge'],1,4096,'temporal edge');need(edge not in self.wires and len(self.wires)<4096,'Temporal wire inventory')
            info={'keys':[],'snapshot':None,'row':row}
            if candidate=='raft-rs':
                fields=V['protobuf'](raw,{1:0,2:0,3:0,4:0,5:0,6:0,7:2,8:0,9:2,10:0,11:0,12:2,13:0,14:0,15:0,16:0},(7,))
                for encoded in fields.get(7,[]):
                    entry=V['protobuf'](encoded,{1:0,2:0,3:0,4:2,5:0,6:2});kind=integer(V['scalar'](entry,1),0,2,'temporal entry type')
                    key=(integer(V['scalar'](entry,3),1,4096,'temporal index'),integer(V['scalar'](entry,2),0,2**63-1,'temporal term'),0)
                    data=V['scalar'](entry,4,b'');operations=V['batch'](V['decode'](data))if kind==0 and data else {}
                    change=None
                    if kind==1:
                        change_fields=V['protobuf'](data,{1:0,2:0,3:0,4:2})
                        change=('legacy',integer(V['scalar'](change_fields,2),0,2,'temporal conf type'),integer(V['scalar'](change_fields,3),1,4,'temporal conf node'))
                    elif kind==2:
                        change_fields=V['protobuf'](data,{1:0,2:2,3:2},(2,));changes=[]
                        for encoded_change in change_fields.get(2,[]):
                            item=V['protobuf'](encoded_change,{1:0,2:0});changes.append((integer(V['scalar'](item,1),0,2,'temporal conf type'),integer(V['scalar'](item,2),1,4,'temporal conf node')))
                        need(len(changes)<=4,'Temporal conf change count')
                        change=('v2',integer(V['scalar'](change_fields,1),0,2,'temporal conf transition'),changes)
                    self.define(key,raft_token(entry),operations,change,row);info['keys'].append(key)
                if 9 in fields:
                    snapshot=V['protobuf'](V['scalar'](fields,9),{1:2,2:2});meta=V['protobuf'](V['scalar'](snapshot,2,b''),{1:2,2:0,3:0})
                    key=(integer(V['scalar'](meta,2),1,4096,'temporal snapshot index'),integer(V['scalar'](meta,3),0,2**63-1,'temporal snapshot term'),0)
                    app=V['decode'](V['scalar'](snapshot,1,b''));need(app['applied']==key[0],'Temporal serialized snapshot prefix')
                    conf=V['protobuf'](V['scalar'](meta,1,b''),{1:0,2:0,3:0,4:0,5:0},(1,2,3,4))
                    sets=[conf.get(1,[])]+([conf[3]]if conf.get(3)else [])
                    info['snapshot']=(key,self.snapshot_operations(app['operations']),configurations(sets))
            else:
                request=V['decode'](raw)
                if row['message']=='append_request':
                    for entry in request['entries']:
                        key=log_id(entry['log_id']);payload=entry['payload'];operations={};change=None
                        if type(payload)is dict and 'Normal'in payload:operations=V['batch'](payload['Normal'])
                        elif type(payload)is dict and 'Membership'in payload:
                            change=('open',open_membership(payload['Membership']))
                            self.open_memberships[key]=payload['Membership']
                        else:need(payload=='Blank','Temporal OpenRaft payload')
                        token=hashlib.sha256(json.dumps(entry,sort_keys=True,separators=(',',':')).encode()).hexdigest()
                        self.define(key,token,operations,change,row);info['keys'].append(key)
                elif row['message']=='snapshot_request' and not row['dropped']:
                    meta=request['meta'];chunk_key=(row['target'],meta['snapshot_id']);need(len(chunks)<8 or chunk_key in chunks,'Temporal snapshot stream count')
                    chunk=chunks.setdefault(chunk_key,bytearray());need(type(request['offset'])is int and request['offset']==len(chunk),'Temporal snapshot chunk offset')
                    data=request['data'];need(type(data)is list and len(data)<=1048576 and all(type(v)is int and 0<=v<=255 for v in data),'Temporal snapshot chunk bytes')
                    need(len(chunk)+len(data)<=1048576,'Temporal snapshot stream byte bound');chunk.extend(data)
                    if request['done']is True:
                        app=V['decode'](bytes(chunk));key=log_id(meta['last_log_id']);need(log_id(app['last'])==key and app['membership']==meta['last_membership'],'Temporal snapshot metadata/content')
                        info['snapshot']=(key,self.snapshot_operations(app['operations']),open_membership(meta['last_membership']['membership']));del chunks[chunk_key]
            self.wires[edge]=info

    def define(self,key,token,operations,change,row):
        need(len(self.definitions)<4096 or key in self.definitions,'Temporal unique entry bound')
        definition={'token':token,'operations':operations,'ids':tuple(operations),'change':change}
        need(key not in self.definitions or self.definitions[key]==definition,'Same temporal log identity has different bytes')
        self.definitions[key]=definition;self.origin.setdefault(key,row['source']);self.first_wire.setdefault(key,row['sequence'])

    def snapshot_operations(self,value):
        need(type(value)is dict and len(value)<=128,'Temporal snapshot operation bound')
        result={}
        for identity,operation in value.items():
            actual,body=V['operation'](operation);need(identity==actual,'Temporal snapshot operation key');result[identity]=body
        return result

    def configuration_before(self,index):
        return self.voters[max(i for i in self.voters if i<index)]

    def changed_configuration(self,previous,change):
        if change is None:return previous
        if change[0]=='open':return change[1]
        if change[0]=='legacy':
            need(len(previous)==1,'Legacy membership change inside joint state');changes=[change[1:]];transition=0
        else:
            transition,changes=change[1:]
            if not changes:
                need(len(previous)==2,'Leave-joint without joint state');return (previous[0],)
            need(len(previous)==1,'Enter-joint while already joint')
        incoming=set(previous[0])
        for kind,node in changes:
            if kind==0:incoming.add(node)
            else:incoming.discard(node)
        need(incoming,'Temporal membership removes every voter')
        if change[0]=='v2' and (transition!=0 or len(changes)>1):return (frozenset(incoming),previous[0])
        return (frozenset(incoming),)

    def has(self,node,key):
        index=key[0]
        directory=f'node-{node}'+('-log' if self.candidate=='openraft' else '')
        disk=self.transactions.disks.get(directory)
        if disk is None or disk['poisoned'] or disk['pending'] is not None or directory in self.quarantined:return False
        return (index<=self.base[node] and self.by_index.get(index)==key) or (
            self.entries[node].get(index)==key and self.tail[node]>=index and key in self.definitions and
            self.entry_tokens[node].get(index)==self.definitions[key]['token'])

    def certify(self,key,sequence):
        need(key in self.definitions and self.first_wire[key]<sequence,'Committed application precedes serialized entry')
        if key in self.certificates:return
        index=key[0];need(index==self.prefix+1,'Temporal committed prefix has a gap or conflicting identity')
        definition=self.definitions[key];previous=self.configuration_before(index);after=self.changed_configuration(previous,definition['change'])
        # Pinned OpenRaft membership is effective when its log is viewed;
        # the membership entry itself uses its new (possibly joint) sets.
        # The fixture serializes membership changes and normal writes.
        required=list(after if definition['change']is not None and definition['change'][0]=='open'else previous)
        for voters in required:
            need(sum(self.has(node,key)for node in voters)>=len(voters)//2+1,'Application lacks prior actual durable quorum')
        for identity,body in definition['operations'].items():
            need(identity not in self.operations or self.operations[identity]==body,'Committed identity has different operation bytes');self.operations[identity]=body
        need(len(self.operations)<=128,'Temporal committed operation bound')
        self.certificates[key]=sequence;self.by_index[index]=key;self.prefix=index;self.prefix_ids[index]=frozenset(self.operations)
        self.voters[index]=after

    def received_wire(self,info):
        row=info['row'];need(not row['dropped'],'Delivered temporal dropped edge')
        node=integer(row['target'],1,4,'temporal target')
        self.received.update((node,key)for key in info['keys'])
        if info['snapshot']is not None:
            key,operations,sets=info['snapshot'];need(self.by_index.get(key[0])==key and
                frozenset(operations)==self.prefix_ids[key[0]] and all(self.operations[k]==v for k,v in operations.items()) and
                sets==self.voters[key[0]],'Received snapshot lacks certified exact prefix/configuration')
            self.received_snapshots[node]=(key,frozenset(operations))

    def recompute_tail(self,node):
        tail=self.base[node]
        while tail+1 in self.entries[node]:tail+=1
        self.tail[node]=tail

    def callback(self,node,keys,sequence):
        need(len(keys)<=256,'Temporal committed callback bound')
        for key in keys:
            self.certify(key,sequence)
            need(key[0]not in self.callbacks[node]or self.callbacks[node][key[0]]==key,'Committed callback conflicts with earlier prefix')
            self.callbacks[node][key[0]]=key;self.callback_count+=1

    def snapshot(self, saved):
        if self.candidate == 'raft-rs':
            fields=V['protobuf'](bytes_value(saved), {1:2,2:2})
            meta=V['protobuf'](V['scalar'](fields,2,b''), {1:2,2:0,3:0})
            key=raft_id([V['scalar'](meta,2), V['scalar'](meta,3)])
            app=V['decode'](V['scalar'](fields,1,b''))
            need(type(app) is dict and set(app)=={'applied','operations'} and
                 app['applied']==key[0], 'Snapshot application prefix/content')
            sets=conf_sets(V['scalar'](meta,1,b''))
        else:
            need(type(saved) is dict and set(saved)=={'meta','data'}, 'Stored snapshot shape')
            meta=saved['meta'];need(type(meta) is dict and
                 set(meta)=={'last_log_id','last_membership','snapshot_id'}, 'Snapshot metadata shape')
            need(type(meta['snapshot_id']) is str and 1<=len(meta['snapshot_id'])<=128 and meta['snapshot_id'].isascii(),
                 'Snapshot identifier bound')
            key=log_id(meta['last_log_id']);app=V['decode'](bytes_value(saved['data']))
            need(type(app) is dict and set(app)=={'last','membership','operations'} and
                 log_id(app['last'])==key and app['membership']==meta['last_membership'], 'Snapshot tuple/content')
            sets=open_membership(app['membership']['membership'])
        return key,self.snapshot_operations(app['operations']),sets

    def snapshot_base(self,node,key,operations,sets):
        need(self.by_index.get(key[0])==key and frozenset(operations)==self.prefix_ids[key[0]] and
             all(self.operations[k]==v for k,v in operations.items()) and self.voters[key[0]]==sets,
             'Persisted snapshot lacks independently certified content/configuration')
        local=self.frames[node][0]>=key[0]
        incoming=self.received_snapshots.get(node)
        need(local or incoming==(key,frozenset(operations)), 'Persisted snapshot lacks prior applied/delivered source')

    def install_entries(self,node,base,entries):
        need(len(entries)<=256, 'Completed transaction entry count')
        indices=sorted(entries)
        need(indices==list(range(base+1,base+1+len(indices))), 'Completed transaction durable prefix gap')
        need(base>=self.base[node], 'Completed transaction regressed snapshot/purge base')
        for index,(key,token) in entries.items():
            if index<=self.frames[node][0]:
                need(self.by_index.get(index)==key, 'Completed image replaced an applied log identity')
            if key in self.definitions:
                need(token==self.definitions[key]['token'], 'Persisted Entry body differs from serialized identity')
                need((node,key) in self.received or self.origin.get(key)==node,
                     'Persisted Entry lacks delivered/local serialized content')
            need(index>base, 'Completed Entry overwrote compacted prefix')
        self.base[node]=base
        self.entries[node]={i:key for i,(key,_) in entries.items()}
        self.entry_tokens[node]={i:token for i,(_,token) in entries.items()}
        self.recompute_tail(node)
        need(self.frames[node][0]<=self.tail[node], 'Completed image truncated applied prefix without snapshot')

    def application(self,node,index,operations,sets,identity=None):
        previous=self.frames[node][0]
        need(previous<=index<=self.prefix, 'Synced application exceeds committed prefix or regresses')
        start=previous+1
        incoming=self.received_snapshots.get(node)
        if incoming is not None and incoming[0][0]>=start:start=incoming[0][0]+1
        for position in range(start,index+1):
            need(self.callbacks[node].get(position)==self.by_index.get(position) and position in self.by_index,
                 'Synced application lacks prior committed callback')
        need(frozenset(operations)==self.prefix_ids[index] and
             all(self.operations[k]==v for k,v in operations.items()), 'Synced application bytes differ from committed prefix')
        if index>=0 and self.candidate=='openraft':
            need(identity==self.by_index[index], 'Synced application log identity differs')
        if index>=(1 if self.candidate=='raft-rs' else 0):
            need(sets==self.voters[index], 'Synced applied membership differs from committed configuration')
        self.frames[node]=(index,frozenset(operations));self.application_frames+=1

    def frame(self,row,saved):
        name=row['directory'];match=re.fullmatch(r'node-([1-4])(?:-(log|machine))?',name)
        need(match is not None and type(saved) is dict, 'Completed disk image shape')
        node=int(match[1]);entries={};snapshot=None
        # The original deliberate persisted-snapshot corruption remains visible,
        # but can never establish a new prefix or authorize later declarations.
        stored_snapshot=saved.get('snapshot')
        if stored_snapshot:
            try:snapshot=self.snapshot(stored_snapshot)
            except ValueError:
                old=self.images.get(name)
                need(old is not None and stored_snapshot!=old.get('snapshot') and
                     {k:v for k,v in saved.items() if k!='snapshot'}=={k:v for k,v in old.items() if k!='snapshot'},
                     'Malformed snapshot changed other durable content')
                self.quarantined.add(name);return
        need(name not in self.quarantined, 'Quarantined snapshot reused')
        if self.candidate=='raft-rs':
            need(match[2] is None and set(saved)=={'schema_version','hard_state','entries','conf_state','snapshot','application'} and
                 type(saved['schema_version']) is int and saved['schema_version']==2, 'Completed Raft scratch schema')
            hard=V['protobuf'](bytes_value(saved['hard_state']),{1:0,2:0,3:0})
            base=0
            if snapshot:
                key,ops,sets=snapshot;self.snapshot_base(node,key,ops,sets);base=key[0]
                self.snapshots[node]=key
            need(type(saved['entries']) is list and len(saved['entries'])<=256, 'Completed Raft entry bounds')
            for entry in saved['entries']:
                need(type(entry) is dict and set(entry)=={'kind','term','index','data','context','sync_log'}, 'Completed Raft Entry fields')
                key=raft_id([entry['index'],entry['term']]);kind=integer(entry['kind'],0,2,'stored Entry kind')
                need(type(entry['sync_log']) is bool and key[1]<=V['scalar'](hard,1), 'Stored Entry flags/vote')
                fields={1:[kind],2:[key[1]],3:[key[0]],4:[stored_data(entry['data'])],
                        5:[int(entry['sync_log'])],6:[bytes_value(entry['context'])]}
                need(key[0] not in entries, 'Duplicate completed Entry')
                entries[key[0]]=(key,raft_token(fields))
            need(list(entries)==sorted(entries), 'Completed Raft entry order')
            self.install_entries(node,base,entries)
            app=saved['application'];need(type(app) is dict and set(app)=={'applied','operations'}, 'Completed application shape')
            index=integer(app['applied'],0,4096,'stored applied index')
            need(index<=self.tail[node] and V['scalar'](hard,3)<=self.tail[node], 'Completed committed/applied log bounds')
            ops=self.snapshot_operations(app['operations']);sets=conf_sets(bytes_value(saved['conf_state']))
            summary={'candidate':'raft-rs','index':index,'ids':sorted(ops)}
            self.application(node,index,ops,sets)
        elif match[2]=='log':
            need(set(saved)=={'vote','purged','committed','entries'}, 'Completed OpenRaft log fields')
            vote=saved['vote']
            if vote is not None:
                need(type(vote) is dict and set(vote)=={'leader_id','committed'} and type(vote['committed']) is bool,
                     'Completed OpenRaft vote shape')
                log_id({'leader_id':vote['leader_id'],'index':0})
            base=-1
            if saved['purged'] is not None:
                key=log_id(saved['purged']);base=key[0]
                machine=self.images.get(f'node-{node}-machine')
                need(machine is not None and machine['snapshot'] is not None, 'Purged prefix has no completed local snapshot')
                snap=self.snapshot(machine['snapshot']);self.snapshot_base(node,*snap)
                need(key==self.by_index.get(base) and base<=snap[0][0], 'Purge differs from certified local snapshot')
            need(type(saved['entries']) is dict and len(saved['entries'])<=256, 'Completed OpenRaft entry bounds')
            for position,entry in saved['entries'].items():
                need(type(position) is str and position.isascii() and position.isdecimal() and
                     len(position)<=4 and str(int(position))==position and type(entry) is dict and
                     set(entry)=={'log_id','payload'}, 'Completed OpenRaft Entry shape')
                key=log_id(entry['log_id']);need(key[0]==int(position), 'Completed Entry map identity')
                payload=entry['payload']
                need(payload=='Blank' or type(payload) is dict and len(payload)==1 and
                     set(payload) in ({'Normal'},{'Membership'}), 'Completed OpenRaft payload kind')
                if type(payload) is dict and 'Normal' in payload:V['batch'](payload['Normal'])
                if type(payload) is dict and 'Membership' in payload:open_membership(payload['Membership'])
                token=hashlib.sha256(json.dumps(entry,sort_keys=True,separators=(',',':')).encode()).hexdigest()
                entries[key[0]]=(key,token)
            self.install_entries(node,base,entries)
            if saved['committed'] is not None:
                key=log_id(saved['committed']);need(self.has(node,key), 'Stored OpenRaft commit lacks exact durable prefix')
            summary=None
        else:
            need(match[2]=='machine' and set(saved)=={'applied','snapshot'}, 'Completed OpenRaft machine fields')
            app=saved['applied'];need(type(app) is dict and set(app)=={'last','membership','operations'}, 'Completed OpenRaft application fields')
            identity=None if app['last'] is None else log_id(app['last']);index=-1 if identity is None else identity[0]
            stored=app['membership'];need(type(stored) is dict and set(stored)=={'log_id','membership'}, 'Stored applied membership')
            if stored['log_id'] is None:
                need(index==-1 and stored['membership']=={'configs':[],'nodes':{}}, 'Initial applied membership')
                sets=()
            else:
                member_key=log_id(stored['log_id']);need(member_key[0]<=index and self.by_index.get(member_key[0])==member_key,
                    'Applied membership identity lacks committed entry')
                sets=open_membership(stored['membership'])
                definition=self.definitions[member_key]
                need(definition['change']==('open',sets) and stored['membership']==self.open_memberships.get(member_key),
                     'Applied membership differs from entry contents')
            ops=self.snapshot_operations(app['operations'])
            if snapshot:self.snapshot_base(node,*snapshot)
            self.application(node,index,ops,sets,identity)
            summary={'candidate':'openraft','log_id':app['last'],'ids':sorted(ops)}
        need(row.get('application')==summary, 'Declared application differs from completed transaction contents')
        self.images[name]=saved

    def observe(self,row):
        kind=row['kind'];node=row.get('node');sequence=row['sequence']
        saved=self.transactions.observe(row)
        if kind=='disk_persisted':
            self.frame(row,saved)
            return
        if node is not None:integer(node,1,4,'temporal node')
        if kind=='wire'and self.candidate=='openraft'and not row['dropped']:self.received_wire(self.wires[row['edge']])
        elif kind=='step_received'and self.candidate=='raft-rs':
            info=self.wires.get(row['edge']);need(info is not None and info['row']['sequence']<sequence and info['row']['target']==node,'Temporal delivered edge differs');self.received_wire(info)
        elif kind=='ready_persisted'and self.candidate=='raft-rs':
            saved=self.transactions.current(row,f'node-{node}')
            need(f'node-{node}' not in self.quarantined, 'Ready used corrupt snapshot')
            indices=row['entries'];terms=row.get('entry_terms');need(type(terms)is list and len(indices)==len(terms),'Temporal persisted terms missing')
            snapshot=row.get('persisted_snapshot')
            need((snapshot is None and not saved['snapshot']) or
                 snapshot is not None and self.snapshot(saved['snapshot'])[0]==raft_id(snapshot), 'Ready snapshot declaration differs from content')
            need(len(indices)<=256 and all(self.has(node,raft_id([index,term])) for index,term in zip(indices,terms)),
                 'Ready declared entries absent from completed contents')
            need(type(row.get('committed_entries'))is list and all(self.callbacks[node].get(raft_id(item)[0])==raft_id(item)for item in row['committed_entries']),'Ready committed callback witness differs')
        elif kind=='append_readable_and_persisted'and self.candidate=='openraft':
            saved=self.transactions.current(row,f'node-{node}-log')
            keys=[log_id(value)for value in row.get('log_ids',[])];need([key[0]for key in keys]==row['indices'],'Temporal persisted OpenRaft identities missing')
            need(row.get('purged')==saved['purged'] and len(keys)<=256 and all(self.has(node,key) for key in keys),
                 'Append declared entries/purge absent from completed contents')
        elif kind=='snapshot_persisted'and self.candidate=='raft-rs':
            saved=self.transactions.current(row,f'node-{node}')
            need(self.snapshot(saved['snapshot'])[0]==raft_id([row['index'],row.get('term')]), 'Snapshot declaration differs from completed contents')
        elif kind=='snapshot_persisted'and self.candidate=='openraft':
            saved=self.transactions.current(row,f'node-{node}-machine')
            need(self.snapshot(saved['snapshot'])[0][0]==row['index'], 'Snapshot declaration differs from completed contents')
        elif kind=='committed_apply_received':
            keys=[raft_id(value)for value in row['entries']]if self.candidate=='raft-rs'else[log_id(value)for value in row['log_ids']]
            self.callback(node,keys,sequence)
        elif kind=='advance_append'and self.candidate=='raft-rs':
            need(type(row.get('light_committed_entries'))is list,'LightReady committed callback witness missing')
            self.callback(node,[raft_id(value)for value in row['light_committed_entries']],sequence)
        elif kind=='applied':
            directory=f'node-{node}'+('-machine' if self.candidate=='openraft' else '')
            self.transactions.current(row,directory)
            need(directory not in self.quarantined, 'Application used corrupt snapshot')
            key=raft_id([row['index'],row.get('term')])if self.candidate=='raft-rs'else log_id(row.get('log_id'))
            need(self.callbacks[node].get(key[0])==key and key in self.certificates,'Applied entry lacks prior committed callback/quorum')
            need(self.has(node,key),'Applied entry is not in the node durable prefix')
            prefix,ids=self.frames[node];need(prefix>=key[0]and set(row['ids'])<=ids and tuple(row['ids'])==self.definitions[key]['ids'],'Application precedes its actual synced image/identity')
            self.applied+=1
        elif kind=='state_witness':
            disk=self.transactions.disks.get(row['directory'])
            need(disk is not None and V['extent'](row,self.transactions.files)==disk['raw'],
                 'Read witness differs from completed transaction contents')
        elif kind=='truncated_snapshot_refused':
            directory=f'node-{node}'+('-machine' if self.candidate=='openraft' else '')
            need(directory in self.quarantined, 'Snapshot refusal lacks actual malformed completed snapshot')
            self.quarantine_observed.add(directory)


def verify(rows,candidate,files):
    history=History(rows,candidate,files)
    for row in rows:history.observe(row)
    history.transactions.finish()
    need(history.quarantined==history.quarantine_observed, 'Malformed snapshot not refused/quarantined')
    need(history.applied>0 and history.callback_count>0 and history.application_frames>0,'Temporal application evidence missing')
    return {'temporal_committed_application':'pass','certified_entries':len(history.certificates),
            'application_frames':history.application_frames,'committed_callbacks':history.callback_count,'applied_events':history.applied}
