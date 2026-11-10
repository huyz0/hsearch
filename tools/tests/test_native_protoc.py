"""HOST metadata/protoc controls. No Cargo, compiler or protoc process runs."""
import copy
import hashlib
import json
import os
from pathlib import Path
import runpy
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

M=runpy.run_path(str(Path(__file__).resolve().parents[1]/'native_protoc.py'))
G=M['prepare'].__globals__


class NativeProtocControls(unittest.TestCase):
    def fixture(self,root):
        source=root/'source';source.mkdir();(source/'dev').mkdir()
        (source/'dev/compile_entry.py').write_bytes(b'# fixed HOST adapter fixture\n')
        archive=root/'package.crate';archive.write_bytes(b'fixed synthetic archive')
        package=root/'package';package.mkdir();(package/'bin').mkdir();(package/'src').mkdir()
        tool=package/'bin/protoc-linux-aarch_64';tool.write_bytes(b'fixed synthetic native ELF fixture')
        (package/'Cargo.toml').write_bytes(b'[package]\nname="fixture"\n')
        (package/'src/protobuf_impl.rs').write_bytes(b'// fixed selector fixture\n')
        archive_sha=hashlib.sha256(archive.read_bytes()).hexdigest()
        (source/'Cargo.lock').write_text('version = 4\n[[package]]\nname="protobuf-build"\nversion="0.14.1"\nsource="registry+fixture"\nchecksum="'+archive_sha+'"\n')
        cache=root/'cache';cache.mkdir();output=root/'output';output.mkdir()
        def file_proof(path,*args):return {'sha256':hashlib.sha256(path.read_bytes()).hexdigest(),'bytes':path.stat().st_size}
        proof={'archive':str(archive),'archive_proof':file_proof(archive),'manifest':str(package/'Cargo.toml'),
               'cache_files':{name:file_proof(package/name) for name in ['Cargo.toml','src/protobuf_impl.rs','bin/'+tool.name]},
               'tool_actual_sha256':file_proof(tool)['sha256'],'native_system':'Linux','native_machine':'aarch64','elf_machine':183}
        calls=[]
        def association(metadata):
            self.assertEqual(metadata,{'packages':[{'name':'protobuf-build'}]})
            calls.append('authenticate-archive-cache-ELF')
            return tool,proof
        library={'association':association,'file_proof':file_proof,'REGISTRY':'registry+fixture','ARCHIVE_SHA':archive_sha,'MAX_ARCHIVE':32*1024**2}
        def command(argv,**options):
            calls.append((argv,options));return json.dumps({'packages':[{'name':'protobuf-build'}]})
        runner=SimpleNamespace(root=output,command=command)
        return source,cache,tool,library,runner,calls

    def test_native_metadata_is_bounded_and_only_fixed_read_only_authentication_runs(self):
        with tempfile.TemporaryDirectory() as t:
            root=Path(t).resolve();source,cache,tool,library,runner,calls=self.fixture(root)
            with patch.dict(G,ROOT=source),patch.object(G['runpy'],'run_path',return_value=library):
                env,context=M['prepare'](runner,{'CARGO_HOME':str(cache),'FIXTURE':'kept'})
                self.assertEqual(env['PROTOC'],str(tool));self.assertEqual(env['FIXTURE'],'kept')
                self.assertEqual(calls[0][0],['cargo','metadata','--locked','--offline','--all-features','--format-version','1'])
                self.assertEqual((calls[0][1]['timeout'],calls[0][1]['limit']),(60,4*1024**2))
                self.assertEqual(calls[1],'authenticate-archive-cache-ELF')
                M['verify'](context)
                summary=json.loads((runner.root/'summary.json').read_bytes())
                self.assertEqual(summary['native_machine'],'aarch64')
                self.assertNotIn('cache_files',summary)

    def test_changed_binary_source_archive_or_selector_refuses_reuse(self):
        for changed in ('tool','adapter','archive','selector'):
            with self.subTest(changed=changed),tempfile.TemporaryDirectory() as t:
                source,cache,tool,library,runner,calls=self.fixture(Path(t).resolve())
                with patch.dict(G,ROOT=source),patch.object(G['runpy'],'run_path',return_value=library):
                    env,context=M['prepare'](runner,{'CARGO_HOME':str(cache)})
                    path={'tool':tool,'adapter':source/'dev/compile_entry.py','archive':Path(context['proof']['archive']),
                          'selector':Path(context['proof']['manifest']).parent/'src/protobuf_impl.rs'}[changed]
                    path.write_bytes(b'changed HOST fixture')
                    with self.assertRaises(ValueError):M['verify'](context)

    def test_conflicting_selector_or_wrong_lock_pin_refuses_before_native_use(self):
        for failure in ('selector','lock'):
            with self.subTest(failure=failure),tempfile.TemporaryDirectory() as t:
                source,cache,tool,library,runner,calls=self.fixture(Path(t).resolve())
                env={'CARGO_HOME':str(cache)}
                if failure=='selector':env['PROTOC']='/unqualified/protoc'
                else:
                    lock=source/'Cargo.lock';lock.write_text(lock.read_text().replace('0.14.1','0.14.2'))
                with patch.dict(G,ROOT=source),patch.object(G['runpy'],'run_path',return_value=library),self.assertRaises(ValueError):
                    M['prepare'](runner,env)
                self.assertFalse((runner.root/'summary.json').exists())
