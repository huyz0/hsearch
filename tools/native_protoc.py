"""Authenticate the retained native protoc for ordinary CI without compiling.

Only the bounded archive/cache/ELF validation functions of the fixed guest
adapter are reused. Its compiler entry and guest environment builder never run
on the host. Actual compilation remains in the existing pinned CI runner.
"""
import hashlib
import json
import os
from pathlib import Path
import runpy
import time
import tomllib
import uuid

ROOT = Path(__file__).resolve().parents[1]


def need(value, reason):
    if not value:
        raise ValueError(reason)


def prepare(runner, environment):
    started=time.monotonic()
    adapter=ROOT/'dev/compile_entry.py'
    raw=adapter.read_bytes()
    need(adapter.resolve()==adapter and 0<len(raw)<=65536,'Native protoc adapter source redirected/oversized')
    library=runpy.run_path(str(adapter))
    cache=Path(environment.get('CARGO_HOME',str(Path.home()/'.cargo'))).absolute()
    need(cache.resolve()==cache and cache.is_dir(),'Native protoc cache redirected/unavailable')
    # Each run_path has private globals. No installed crate or selector is edited.
    library['association'].__globals__['CACHE']=cache
    text=runner.command(['cargo','metadata','--locked','--offline','--all-features','--format-version','1'],
                        env=environment,timeout=60,limit=4*1024**2)
    metadata=json.loads(text)
    lock_raw=(ROOT/'Cargo.lock').read_bytes()
    lock=tomllib.loads(lock_raw.decode())
    selected=[p for p in lock['package'] if p['name']=='protobuf-build']
    need(len(selected)==1 and selected[0]['version']=='0.14.1' and selected[0]['source']==library['REGISTRY'] and
         selected[0]['checksum']==library['ARCHIVE_SHA'],'Native protoc lock pin changed')
    tool,proof=library['association'](metadata)
    need(not environment.get('PROTOC') or environment['PROTOC']==str(tool),'Conflicting native PROTOC selector')
    need(time.monotonic()-started<=120,'Native protoc authentication deadline exceeded')
    context={'adapter':adapter,'adapter_sha256':hashlib.sha256(raw).hexdigest(),
             'lock_sha256':hashlib.sha256(lock_raw).hexdigest(),'tool':tool,'proof':proof,'library':library}
    verify(context)
    summary={'schema':1,'scope':'Native protoc authentication only; no compilation or campaign qualification',
             'tool_sha256':proof['tool_actual_sha256'],'archive_sha256':proof['archive_proof']['sha256'],
             'native_system':proof['native_system'],'native_machine':proof['native_machine'],
             'elf_machine':proof['elf_machine'],'source_adapter_sha256':context['adapter_sha256'],
             'lock_sha256':context['lock_sha256']}
    (runner.root/'summary.json').write_text(json.dumps(summary,indent=2)+'\n',encoding='utf-8')
    return dict(environment,PROTOC=str(tool)),context


def verify(context):
    library=context['library'];proof=context['proof']
    need(hashlib.sha256(context['adapter'].read_bytes()).hexdigest()==context['adapter_sha256'] and
         hashlib.sha256((ROOT/'Cargo.lock').read_bytes()).hexdigest()==context['lock_sha256'],
         'Native protoc source binding changed')
    need(library['file_proof'](context['tool'])==proof['cache_files']['bin/'+context['tool'].name] and
         library['file_proof'](Path(proof['archive']),library['MAX_ARCHIVE'])==proof['archive_proof'] and
         library['file_proof'](Path(proof['manifest']))==proof['cache_files']['Cargo.toml'] and
         library['file_proof'](Path(proof['manifest']).parent/'src/protobuf_impl.rs')==proof['cache_files']['src/protobuf_impl.rs'],
         'Authenticated native protoc/cache input changed')


def prepare_for_ci(coverage, environment):
    output=Path(os.environ['CARGO_TARGET_DIR']).absolute().parent/('hsearch-native-protoc-'+uuid.uuid4().hex)
    runner=coverage['Runner'](output)
    return prepare(runner,environment)
