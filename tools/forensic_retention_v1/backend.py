"""Local Docker transport with one probe/worker and bounded raw output.

Author-local tests inject the transport port. A live invocation still needs
the reviewed source, canonical resource admission and fresh exact84 authority.
"""
import os
from pathlib import Path
import re
import time

from ._bytes import need
from . import protocol as F
from .storage import decode
from ._bytes import regular
from .capture import RawBudget, bounded_capture
from .lifecycle import driver_digest


class DockerBackend:
    def __init__(self, task_api, source_package, *, context, engine_id, resource_check,capture=None):
        self.capture = capture or bounded_capture
        self.source_package = Path(source_package).absolute()
        need(self.source_package.resolve() == self.source_package, 'backend-source')
        need(type(context) is str and re.fullmatch('[A-Za-z0-9_.-]{1,64}', context), 'docker-context')
        self.prefix = ['docker', '--context', context]
        self.engine_id = engine_id
        self.resource_check = resource_check
        self.budget=None

    @property
    def raw(self):
        return self.budget.used if self.budget else 0

    def bind_raw(self,budget,meter):
        if getattr(self,'raw_mode',None)=='operation':need(budget is self.budget,'raw-budget-rebind')
        self.budget=budget
        self.meter=meter
        self.raw_mode='operation'

    def bind_preparation(self,meter):
        need(self.budget is None,'raw-budget-rebind')
        state=[0]
        def save(previous,value):
            need(state[0]==previous,'raw-state-raced');state[0]=value
        self.bind_raw(RawBudget(lambda:state[0],save,meter.check),meter)
        self.raw_mode='preparation'

    def command(self, args, meter=None):
        need(self.budget is not None,'raw-budget-not-bound')
        if self.raw_mode=='preparation':need(args[0] in {'info','context','ps','image'},'readonly-preparation-command')
        remaining = 8192-self.raw
        need(remaining > 0, 'backend-raw-bound')
        timeout = 30
        meter = self.meter if meter is None else meter
        if meter is not None:
            meter.check()
            timeout = min(timeout, max(0.001, (meter.deadline-meter.clock())/1000))
        result = self.capture(self.prefix+args, timeout=timeout, budget=self.budget)
        return result

    def absent(self, ids):
        self.assert_engine()
        for cid in ids:
            F.sha(cid)
            need(not self.command(['ps', '--all', '--no-trunc', '--filter', 'id='+cid, '--format', '{{.ID}}']).strip(),
                 'closed-container-present')

    def assert_engine(self, meter=None):
        info=self.command(['info','--format','{{.ID}} {{.OSType}} {{.CgroupVersion}}'],meter).strip().split()
        need(info==[self.engine_id,'linux','2'],'backend-engine')
        endpoint=self.command(['context','inspect','--format','{{.Endpoints.docker.Host}}'],meter).strip()
        need(endpoint.startswith(('unix://','npipe://')),'backend-remote')

    def preflight(self,manifest,meter=None):
        need(driver_digest(self.source_package)==manifest['source_binding']['driver_fingerprint'],'mounted-source-changed')
        self.resource_check(manifest)
        self.assert_engine(meter)
        expected=manifest['source_binding']['image_id']
        observed=self.command(['image','inspect','--format','{{.Id}}',expected],meter).strip()
        need(observed==expected,'backend-image')
        self.resource_check(manifest)
        if meter is not None:meter.check()

    def verify(self, archive, manifest, pack_sha, meter, metadata):
        need(not any(r['kind'] == 'journal' and r['value'].get('phase') == 'backend-attempt'
                     for r in metadata.rows()), 'backend-no-redispatch')
        self.resource_check(manifest)  # Existing canonical admission, no new pool.
        self.assert_engine(meter)
        metadata.append('journal', dict(phase='backend-attempt', image=manifest['source_binding']['image_id'],
                                       engine=self.engine_id))
        result = None
        for mode in ['probe', 'audit']:
            meter.check()
            control_paths=[metadata.path_for(k) for k in ['manifest','authorization','admission']]
            paths = [str(self.source_package), str(archive.root/manifest['pack_name'])]+[str(p) for p in control_paths]
            need(all(',' not in p and '\n' not in p for p in paths), 'backend-mount-path')
            args = ['create', '--pull=never', '--network=none', '--read-only', '--cap-drop=ALL',
                    '--security-opt=no-new-privileges', '--pids-limit=256', '--memory=4294967296',
                    '--memory-swap=4294967296', '--cpus=2', '--user=10001:10001',
                    '--tmpfs=/scratch:rw,noexec,nosuid,size=2147483648', '--workdir=/app',
                    '--mount', 'type=bind,src='+paths[0]+',dst=/app/forensic_retention_v1,readonly',
                    '--mount', 'type=bind,src='+paths[1]+',dst=/retained/'+manifest['pack_name']+',readonly']
            for source,name in zip(paths[2:],['manifest','authorization','admission']):
                args+=['--mount','type=bind,src='+source+',dst=/inputs/'+name+'.json,readonly']
            args += [
                    '--entrypoint=python', manifest['source_binding']['image_id'], '-B', '-m',
                    'forensic_retention_v1.worker', mode, pack_sha]
            metadata.append('journal', dict(phase='create-intent', mode=mode))
            cid = self.command(args, meter).strip()
            F.sha(cid)
            metadata.append('journal', dict(phase='created', mode=mode, container_id=cid))
            self.resource_check(manifest)
            need(driver_digest(self.source_package)==manifest['source_binding']['driver_fingerprint'],'mounted-source-changed')
            if mode=='audit':
                # One fixed worker performs a complete pack/range audit. Debit
                # a conservative complete read bound before dispatch.
                meter.charge(3*manifest['payload_plan']['pack_bytes']+2*224852,rescan=True)
                meter.charge(0,rescan=True)
            output = self.command(['start', '--attach', cid], meter).strip().encode()
            # Inspect fixed numeric fields only. Do not buffer arbitrary inspect
            # payloads, environment arrays, mounts or user-controlled labels.
            status = self.command(['inspect', '--format', '{{.State.Status}} {{.State.ExitCode}}', cid], meter).strip()
            need(status == 'exited 0', 'backend-worker-status')
            value = decode(output, 8192)
            if mode == 'probe':
                need(value == dict(kind='forensic-cgroup-probe-v1', cpu_millis=2000, memory_bytes=4294967296,
                                   swap_bytes=0, pids=256, scratch_bytes=2147483648, uid=10001), 'backend-kernel-proof')
                metadata.append('journal', dict(phase='probe-receipt', receipt=value))
            else:
                result = value
            # Exact acknowledged exited ID only, without force or broad prune.
            need(self.command(['rm', cid], meter).strip() == cid, 'backend-cleanup')
            metadata.append('journal', dict(phase='removed', mode=mode, container_id=cid))
        self.resource_check(manifest)
        return result


def canonical_resource_check(registry, *, task_claim, owner, namespace, image_id, engine_id):
    """Bind a pre-existing explicit operation lease; never reserve implicitly."""
    def check(manifest):
        data=decode(regular(registry.path,67108864),canonical=False)
        need(data['schema_version']==1 and len(data['leases'])<=512,'resource-registry-bound')
        row = data['leases'].get(namespace)
        budget=dict(cpu_millis=2000,memory_bytes=4294967296,pids=256,disk_bytes=2147483648)
        need(row is not None and row.get('kind') == 'failed-forensic-retention-v1' and
                 row.get('engine')==engine_id and row.get('budget')==budget and
                 row.get('task_claim') == task_claim and row.get('owner_session') == owner and
                 row.get('phase') == 'ready' and row.get('image_id') == image_id == manifest['source_binding']['image_id'] and
                 row.get('manifest_sha256') == F.digest(manifest) and row.get('limits') == F.LIMITS and
                 F.enc(row['limits'])==F.enc(F.LIMITS),
             'canonical-resource-admission')
    return check
