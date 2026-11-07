"""Explicit coordinator entrypoint. No implicit claims, allowances or retries."""
import argparse
import hashlib
from pathlib import Path
import sys
import time

from . import protocol as F
from ._bytes import need, regular
from ._sync import sync_directory
from .storage import Archive, Metadata, decode
from .lifecycle import load_task_api, TaskLifecycle, ClosedJob
from .backend import DockerBackend, canonical_resource_check
from .controller import Controller, ForensicArchive


def read(path):
    return decode(regular(Path(path),1048576))


def load(config, *, source_root=None):
    F.keys(config,'state_root resource_root claim owner_session archive_root metadata_root closed_record closed_record_sha256 docker_context engine_id resource_namespace image_id artifact_root')
    source_root=Path(__file__).resolve().parents[2] if source_root is None else Path(source_root).absolute()
    # Existing canonical state only. Initialization cannot invent another pool.
    state=Path(config['state_root']).absolute()
    resource=Path(config['resource_root']).absolute()
    need(state.is_dir() and resource.is_dir() and (resource/'registry.json').is_file(),'existing-state-required')
    api=load_task_api(source_root)
    store=api['TaskStore'](state,root=source_root)
    registry_body=decode(regular(store.path,67108864),canonical=False)
    need(registry_body['resource_root']==str(resource.resolve()),'canonical-resource-root')
    lifecycle=TaskLifecycle(api,store,config['claim'],config['owner_session'])
    registry=api['SANDBOX']['Registry'](resource)
    backend=DockerBackend(api,source_root/'tools/forensic_retention_v1',context=config['docker_context'],
                          engine_id=config['engine_id'],resource_check=canonical_resource_check(registry,
                          task_claim=config['claim'],owner=config['owner_session'],namespace=config['resource_namespace'],image_id=config['image_id'],engine_id=config['engine_id']))
    closed=ClosedJob(api,config['closed_record'],config['closed_record_sha256'],registry,backend)
    archive=Archive(config['archive_root'])
    metadata=Metadata(config['metadata_root'])
    need(not metadata.root.is_relative_to(archive.root) and not archive.root.is_relative_to(metadata.root),
         'separate-metadata-root')
    return Controller(archive,metadata,lifecycle,closed,backend)


def publish_manifest(value, root):
    root=Path(root).absolute()
    need(root.resolve()==root and root.is_dir(),'manifest-artifact-root')
    paths=list(root.iterdir())
    need(len(paths)<2 and all(p.name.startswith('manifest-') and p.suffix=='.json' and
                             p.is_file() and not p.is_symlink() for p in paths),'manifest-artifact-slots')
    raw=F.enc(value)
    need(len(raw)+sum(p.stat().st_size for p in paths)<=1048576,'manifest-artifact-bytes')
    pin=hashlib.sha256(raw).hexdigest()
    path=root/('manifest-'+pin+'.json')
    with path.open('xb') as stream:
        need(stream.write(raw)==len(raw),'manifest-artifact-short-write')
        stream.flush()
        import os
        os.fsync(stream.fileno())
    sync_directory(root)
    return dict(kind='manifest-published',manifest_sha256=pin,bytes=len(raw))


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation',choices=['prepare','run','recover','audit'])
    parser.add_argument('--config',required=True)
    parser.add_argument('--evidence',required=True,help='Canonical bounded coordinator evidence JSON')
    args=parser.parse_args(argv)
    try:
        config=read(args.config)
        evidence=read(args.evidence)
        controller=load(config)
        if args.operation=='prepare':
            F.keys(evidence,'source_binding preparation review_gate')
            artifact_root=Path(config['artifact_root']).absolute()
            forbidden=[controller.archive.root,controller.metadata.root,Path(config['state_root']).absolute(),
                       Path(config['resource_root']).absolute(),controller.lifecycle.source_root]
            need(all(not artifact_root.is_relative_to(p) and not p.is_relative_to(artifact_root) for p in forbidden),
                 'private-artifact-root')
            manifest=controller.prepare(evidence['source_binding'],evidence['preparation'],evidence['review_gate'])
            result=publish_manifest(manifest,artifact_root)
        elif args.operation=='run':
            F.keys(evidence,'manifest authorization admission disposition review_gate')
            result=controller.run(**evidence)
        elif args.operation=='recover':
            F.keys(evidence,'disposition review_gate')
            result=controller.recover(**evidence)
        else:
            F.keys(evidence,'disposition review_gate')
            now=time.time_ns()//1000000
            meter=F.Meter(dict(started_ms=now,deadline_ms=now+30000))
            retained=ForensicArchive(controller.archive,controller.metadata,meter,
                       disposition=evidence['disposition'],review_gate=evidence['review_gate'],
                       claim=controller.lifecycle.claim,owner=controller.lifecycle.owner)
            result=dict(kind='opaque-audit',logical=retained.totals(),qualification=False,semantic_export=False)
        raw=F.enc(result)
        need(len(raw)<=16384,'public-output')
        sys.stdout.buffer.write(raw+b'\n')
        return 0
    except Exception:
        sys.stderr.write('forensic-operation-refused; preserve existing evidence\n')
        return 1


if __name__=='__main__':
    raise SystemExit(main())
