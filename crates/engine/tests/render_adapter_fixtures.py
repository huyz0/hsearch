"""Project hash-frozen public cases into production adapter tests, never rewrite oracles."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import subprocess
from pathlib import Path

NAMES = ('uid','tenant','category','title','price','active','created_at')

def lit(value):
    return json.dumps(value, ensure_ascii=False)

def epoch(value):
    return int(datetime.strptime(value,'%Y-%m-%dT%H:%M:%SZ').replace(tzinfo=timezone.utc).timestamp())*1000 if isinstance(value,str) else value

def val(field,value):
    if field == 'price': return f'Value::Integer({value})'
    if field == 'created_at': return f'Value::Date({epoch(value)})'
    if field == 'active': return f'Value::Boolean({str(value).lower()})'
    return f'Value::String({lit(value)})'

def query(q):
    assert len(q)==1
    kind,body=next(iter(q.items()))
    if kind=='match_all': return 'Query::All'
    if kind=='bool':
        parts=[f'{n}: &['+', '.join(query(x) for x in body.get(n,[]))+']' for n in ('must','filter','should','must_not')]
        return 'Query::Bool {'+', '.join(parts)+f', minimum_should_match: {body.get("minimum_should_match",0)}'+'}'
    field,arg=next(iter(body.items()))
    if kind=='term': return f'Query::Term {{field:{lit(field)},value:{val(field,arg)}}}'
    if kind=='terms': return f'Query::Terms {{field:{lit(field)},values:&['+', '.join(val(field,v) for v in arg)+']}'
    if kind in ('match','match_phrase'): return f'Query::Match {{field:{lit(field)},text:{lit(arg)},phrase:{str(kind=="match_phrase").lower()}}}'
    if kind=='range':
        lo,hi=(-2147483648,2147483647) if field=='price' else (-62167219200000,253402300799999)
        if 'gte' in arg: lo=epoch(arg['gte'])
        if 'gt' in arg: lo=epoch(arg['gt'])+1
        if 'lte' in arg: hi=epoch(arg['lte'])
        if 'lt' in arg: hi=epoch(arg['lt'])-1
        return f'Query::Range {{field:{lit(field)},lower:{val(field,lo)},upper:{val(field,hi)}}}'
    raise ValueError(kind)

def held_out(root, envelope):
    raw=(root/'fixtures/engine-research/held-out.json').read_bytes()
    assert len(raw)<=65536 and hashlib.sha256(raw).hexdigest()==envelope['held_out_sha256']
    held=json.loads(raw); phrase=held['phrase']; dates=held['date']
    assert len(phrase['documents'])==3 and len(dates['documents'])==5 and phrase['slop']==0
    out=['#[test] fn production_held_out_phrase_discriminates_unordered_and() {',
         'let directory=ByteBuffersDirectory::new(); let schema=Schema::new(1,&[("title",Kind::Text)]).unwrap(); let engine=Engine::open(&directory,schema.clone(),None).unwrap();']
    for row in phrase['documents']:
        out.append(f'engine.apply(schema.prepare({lit(row["id"])},&[Mutation::Put{{id:{lit(row["id"])},version:1,fields:&[("title",Value::String({lit(row["title"])}))]}}]).unwrap()).unwrap();')
    out.append('let snapshot=engine.refresh().unwrap();')
    out.append(f'let exact=snapshot.search(&Query::Match{{field:"title",text:{lit(" ".join(phrase["query"]))},phrase:true}},&[],128,&[]).unwrap();')
    out.append('assert_eq!(exact.hits.iter().map(|h|h.id.as_str()).collect::<Vec<_>>(),vec!['+', '.join(lit(i) for i in phrase['exact_phrase_ids'])+']);')
    terms=', '.join('Query::Term{field:"title",value:Value::String('+lit(t)+')}' for t in phrase['query'])
    out.append('let unordered=snapshot.search(&Query::Bool{must:&['+terms+'],filter:&[],should:&[],must_not:&[],minimum_should_match:0},&[],128,&[]).unwrap();')
    out.append('assert_eq!(unordered.hits.iter().map(|h|h.id.as_str()).collect::<Vec<_>>(),vec!['+', '.join(lit(i) for i in phrase['unsafe_unordered_and_ids'])+']);')
    out.append('assert_ne!(exact.hits,unordered.hits);engine.close().unwrap();}')
    out+=['#[test] fn production_held_out_date_range_discriminates_match_all() {',
         'let directory=ByteBuffersDirectory::new(); let schema=Schema::new(1,&[("date",Kind::Date)]).unwrap(); let engine=Engine::open(&directory,schema.clone(),None).unwrap();']
    for row in dates['documents']:
        out.append(f'engine.apply(schema.prepare({lit(row["id"])},&[Mutation::Put{{id:{lit(row["id"])},version:1,fields:&[("date",Value::Date({row["epoch_ms"]}))]}}]).unwrap()).unwrap();')
    out.append('let snapshot=engine.refresh().unwrap();')
    out.append(f'let exact=snapshot.search(&Query::Range{{field:"date",lower:Value::Date({dates["minimum"]}),upper:Value::Date({dates["maximum"]})}},&[],128,&[]).unwrap();')
    out.append('assert_eq!(exact.hits.iter().map(|h|h.id.as_str()).collect::<Vec<_>>(),vec!['+', '.join(lit(i) for i in dates['inclusive_range_ids'])+']);')
    out.append('let wrong=snapshot.search(&Query::All,&[],128,&[]).unwrap();')
    out.append('assert_eq!(wrong.hits.iter().map(|h|h.id.as_str()).collect::<Vec<_>>(),vec!['+', '.join(lit(i) for i in sorted(dates['unsafe_match_all_ids']))+']);')
    out.append('assert_eq!(exact.total,3);assert_eq!(wrong.total,5);assert_ne!(exact.hits,wrong.hits);engine.close().unwrap();}')
    return out

def render(root):
    folder=root/'fixtures/api/core-v1'
    envelope=json.loads((root/'docs/plan/research/native-engine-envelope.json').read_bytes())
    data={}
    for name,digest in envelope['source']['fixture_sha256'].items():
        raw=(folder/name).read_bytes()
        assert len(raw)<=65536 and hashlib.sha256(raw).hexdigest()==digest,name
        data[name]=json.loads(raw)
    out=['//! Generated public differential tests of the typed materialized adapter.',
         '// Generated by crates/engine/tests/render_adapter_fixtures.py from frozen public inputs.',
         '// Native typed semantics only. HTTP, authoritative GET and durability are separate tasks.',
         'use hsearch_engine::{Engine, Kind, Mutation, Query, Schema, Sort, StoredValue, Value, analyze_text};',
         'use lucene_store::ByteBuffersDirectory;',
         'fn schema() -> Schema { Schema::new(1,&['+', '.join(f'({lit(n)},Kind::{k})' for n,k in zip(NAMES,('Keyword','Keyword','Keyword','Text','Integer','Boolean','Date')))+']).unwrap() }',
         '#[test] fn public_queries_aggregations_and_projection() {',
         'let directory=ByteBuffersDirectory::new();let schema=schema();let engine=Engine::open(&directory,schema.clone(),None).unwrap();']
    for row in data['documents.json']:
        fields=', '.join(f'({lit(n)},{val(n,row["source"][n])})' for n in NAMES)
        out.append(f'engine.apply(schema.prepare({lit(row["id"])},&[Mutation::Put{{id:{lit(row["id"])},version:1,fields:&[{fields}]}}]).unwrap()).unwrap();')
    out.append('let snapshot=engine.refresh().unwrap();')
    for case in data['search-cases.json']:
        body=case['request']['json']; oracle=case['reference_oracle']; cid=lit(case['id'])
        out.append('{let query='+query(body.get('query',{'match_all':{}}))+';')
        if oracle['kind']=='hits':
            sorts=[]
            for s in body['sort']:
                f,d=next(iter(s.items()))
                sorts.append('Sort::Score' if f=='_score' else f'Sort::Field{{name:{lit(f)},descending:{str(d=="desc").lower()}}}')
            projection=body.get('_source',[])
            out.append('let result=snapshot.search(&query,&['+', '.join(sorts)+f'],{body["size"]},&['+', '.join(lit(n) for n in projection)+']).unwrap();')
            out.append('let expected:Vec<&str>=vec!['+', '.join(lit(i) for i in oracle['ids'])+'];')
            out.append(f'assert_eq!(result.hits.iter().map(|h|h.id.as_str()).collect::<Vec<_>>(),expected,{cid});')
            out.append(f'assert_eq!(result.total,{oracle["exact_total"]},{cid});')
            out.append(f'for hit in result.hits {{ assert_eq!(hit.version,1); assert_eq!(hit.fields.len(),{len(projection)});')
            for n in projection:
                if n=='uid': out.append('assert_eq!(hit.fields[0],("uid".to_string(),StoredValue::String(hit.id)));')
            out.append('}')
        elif case['id']=='finite-metrics':
            m=oracle['values']
            out.append('let m=snapshot.metrics(&query,&["price"]).unwrap().remove(0);')
            out.append(f'assert_eq!((m.sum,m.min,m.max,m.count),({float(m["sum_price"])},Some({float(m["min_price"])}),Some({float(m["max_price"])}),{m["count_price"]}));')
        elif case['id']=='finite-key-ordered-terms':
            buckets=oracle['values']['categories']
            out.append('assert_eq!(snapshot.groups(&query,"category",128).unwrap().iter().map(|b|(b.key.as_str(),b.count)).collect::<Vec<_>>(),vec!['+', '.join(f'({lit(b["key"])},{b["doc_count"]})' for b in buckets)+']);')
        else: raise ValueError(case['id'])
        out.append('}')
    out+=['engine.close().unwrap();','}', '#[test] fn public_analyzer_tokens_positions_and_utf16_offsets() {']
    for c in data['analyzer-cases.json']:
        out.append('{let expected:Vec<(&str,i32,i32,i32)>=vec!['+', '.join(f'({lit(t["token"])},{t["position"]},{t["start_offset"]},{t["end_offset"]})' for t in c['tokens'])+'];')
        out.append(f'assert_eq!(analyze_text({lit(c["text"])}).unwrap().iter().map(|t|(t.term.as_str(),t.position,t.start_offset,t.end_offset)).collect::<Vec<_>>(),expected,{lit(c["id"])});'+'}')
    out+=['}']
    out.extend(held_out(root,envelope))
    return '\n'.join(out)+'\n'

if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',required=True,type=Path)
    parser.add_argument('--check',action='store_true')
    parser.add_argument('--formatter',default='rustfmt')
    args=parser.parse_args()
    expected=render(args.root)
    expected=subprocess.run([args.formatter,'--edition','2024','--emit','stdout'],input=expected,text=True,encoding='utf-8',capture_output=True,check=True).stdout
    target=args.root/'crates/engine/tests/adapter_public.rs'
    if args.check:
        assert target.read_text(encoding='utf-8')==expected
    else: target.write_text(expected,encoding='utf-8',newline='\n')
