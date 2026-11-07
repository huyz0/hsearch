"""Same-checkout TaskStore interlock. No parent task identity override."""
import hashlib
import os
from pathlib import Path
import runpy
import secrets

from ._bytes import need, regular
from . import protocol as F
from .storage import decode, identity

OWNED = ['tools/forensic_retention_v1', 'tools/tests/test_forensic_retention_v1.py']
PHASES = {'forensic-active', 'forensic-interrupted', 'forensic-complete'}


def load_task_api(source_root):
    source_root = Path(source_root).absolute()
    need(source_root.resolve() == source_root, 'source-root-redirect')
    return runpy.run_path(str(source_root/'tools/task.py'))


class TaskLifecycle:
    def __init__(self, api, store, claim, owner):
        self.api, self.store, self.claim, self.owner = api, store, claim, owner
        self.source_checks = self.source_bytes = 0
        record = self.record()
        packet = record['packet']
        need(packet['task_id'] == 'M01.17' and packet['owned_paths'] == OWNED,
             'standalone-task-ownership')
        need(record['namespace'] is None, 'standalone-resource-busy')
        self.source_root = store.inspect_worktree(record)

    def record(self):
        """Atomic published registry snapshot, without edit()'s rewrite side effect."""
        registry = decode(regular(self.store.path, 67108864), canonical=False)
        need(registry['schema_version']==1 and registry['repository']==self.store.repository, 'task-registry-identity')
        row = registry['tasks'].get(self.claim)
        need(row is not None and row['packet']['owner_session']==self.owner and
             row['packet_sha256']==hashlib.sha256(self.api['encoded'](self.api['packet'](row['packet']))).hexdigest(),
             'task-packet-binding')
        expected=self.api['guarded'](self.store.root,'worktrees',self.claim)
        need(Path(row['worktree'])==expected, 'task-worktree-binding')
        return row

    def source(self, image_id):
        # Every source snapshot is independently bounded by TaskStore at
        # 10,000 paths/128MiB plus at most sixteen 1MiB package inputs.
        # Eight snapshots bound this separate source-read pool at 1152MiB.
        need(self.source_checks < 8, 'source-check-count')
        self.source_checks += 1
        record = self.record()
        measured = self.store.fingerprint(record)
        # Bind every executable input through TaskStore and the package driver
        # separately. No import from frozen parent worktrees or private oracles.
        paths = sorted((self.source_root/'tools/forensic_retention_v1').glob('*.py'))
        need(1 <= len(paths) <= 16, 'driver-files')
        driver = [{'path':p.name, 'sha256':hashlib.sha256(regular(p, 1048576)).hexdigest()} for p in paths]
        self.source_bytes += measured['input_bytes'] + sum(p.stat().st_size for p in paths)
        need(self.source_bytes <= 1207959552, 'source-read-bytes')
        return dict(head=measured['head'], tree=self.api['git']('rev-parse', 'HEAD^{tree}', root=self.source_root).strip(),
                    fingerprint=measured['digest'], driver_fingerprint=F.digest(driver),
                    clean=measured['clean'], image_id=image_id)

    def begin(self, manifest, metadata_root):
        need(self.source(manifest['source_binding']['image_id']) == manifest['source_binding'], 'source-changed')
        token = secrets.token_hex(16)
        worker = dict(pid=os.getpid(), identity=self.api['SANDBOX']['owner_identity']())
        need(worker['identity'] not in {None, 'unknown'}, 'controller-birth-unknown')
        with self.store.edit() as data:
            row = data['tasks'][self.claim]
            need(row['packet']['owner_session'] == self.owner and row['phase'] == 'allocated' and
                 row['namespace'] is None and row['worker'] is None and 'forensic' not in row,
                 'standalone-lifecycle-busy')
            row.update(phase='forensic-active', worker=worker,
                       forensic=dict(version=1, token=token, manifest=F.digest(manifest),
                                     metadata_root=str(Path(metadata_root).absolute()),
                                     archive_identity=identity(manifest['payload_plan']['archive_root'])))
        self.token = token

    def check(self, manifest):
        row = self.record()
        need(row['phase'] == 'forensic-active' and row['forensic']['token'] == self.token and
             row['forensic']['manifest'] == F.digest(manifest) and
             row['worker'] == dict(pid=os.getpid(), identity=self.api['SANDBOX']['owner_identity']()),
             'standalone-lifecycle-changed')
        need(self.source(manifest['source_binding']['image_id']) == manifest['source_binding'], 'source-changed')

    def finish(self, success):
        with self.store.edit() as data:
            row = data['tasks'][self.claim]
            need(row['phase'] == 'forensic-active' and row['forensic']['token'] == self.token,
                 'standalone-lifecycle-changed')
            row.update(phase='forensic-complete' if success else 'forensic-interrupted', worker=None)

    def recover(self, manifest, metadata_root):
        """Resume only this same immutable operation. Default TaskStore refuses it."""
        need(self.source(manifest['source_binding']['image_id']) == manifest['source_binding'], 'source-changed')
        with self.store.edit() as data:
            row = data['tasks'][self.claim]
            need(row['phase'] in PHASES-{'forensic-complete'} and
                 not self.api['handle_live'](row['worker']), 'forensic-controller-live-or-unknown')
            need(row['forensic']['manifest'] == F.digest(manifest) and
                 row['forensic']['metadata_root'] == str(Path(metadata_root).absolute()) and
                 row['forensic']['archive_identity'] == identity(manifest['payload_plan']['archive_root']),
                 'forensic-recovery-binding')
            self.token = row['forensic']['token']
            row.update(phase='forensic-active', worker=dict(pid=os.getpid(), identity=self.api['SANDBOX']['owner_identity']()))


class ClosedJob:
    """Read pinned job closure and independently inspect birth/ID/lease absence.

    The coordinator supplies the actual evidence descriptor and authoritative
    existing resource registry. Boolean claims alone are insufficient.
    """
    def __init__(self, task_api, record_path, record_sha256, resource_registry, docker):
        self.api, self.path, self.pin = task_api, Path(record_path).absolute(), record_sha256
        self.registry, self.docker = resource_registry, docker
        self.identity = identity(self.path)

    def check(self):
        need(identity(self.path) == self.identity, 'closed-record-identity')
        row = decode(regular(self.path, 131072, self.pin))
        F.keys(row, 'kind claim owner_session groups phase controller container_ids source_head source_tree cli_exit guest_tests host_proof cause')
        need(row['kind'] == 'failed-forensic-closed-job-v1' and row['claim'] == F.CLAIM and
             row['owner_session'] == F.OWNER and row['groups'] == list(F.PAIR) and
             row['phase'] == 'closed' and row['cli_exit'] == 1 and row['guest_tests'] == 601 and
             row['host_proof'] is False and row['cause'] == 'unconfirmed', 'closed-job-binding')
        need(row['source_head'] == 'fffbe1142717f45d538b2979021719db7f09d302' and
             row['source_tree'] == '86c6636f934d1648747a5d9d7849c2b776f48740', 'closed-job-source')
        handle = row['controller']
        F.keys(handle, 'pid identity')
        need(type(handle['pid']) is int and handle['pid'] > 0 and
             type(handle['identity']) is str and handle['identity'] != 'unknown' and
             not self.api['handle_live'](handle), 'closed-writer-live-or-unknown')
        ids = row['container_ids']
        need(type(ids) is list and 1 <= len(ids) <= 8 and len(set(ids)) == len(ids), 'closed-ids')
        for cid in ids:
            F.sha(cid)
        self.docker.absent(ids)
        registry = decode(regular(self.registry.path, 67108864), canonical=False)
        need(registry['schema_version']==1 and len(registry['leases'])<=512, 'closed-registry-bound')
        need(not any(v.get('task_claim') == F.CLAIM and v.get('phase') != 'closed'
                     for v in registry['leases'].values()), 'closed-lease-live-or-unknown')
        return F.digest(row)


def authority(manifest, authorization, disposition, review_gate, *, owner, claim):
    """Trusted-coordinator evidence protocol, not user authentication.

    No tool or text parser can mint a human decision. The invoking coordinator
    must retain the actual fresh human message and root disposition, just as
    the existing ReviewStore consumes coordinator-observed check receipts.
    """
    F.validate(manifest)
    need(authorization == F.authorization(manifest, authorization['answer']), 'human-binding')
    F.keys(disposition, 'kind task claim owner_session manifest_sha256 source_binding review_packet_sha256 human_message_sha256 archive_identity metadata_root image_id limits')
    need(disposition['kind'] == 'root-exact84-retention-v1' and disposition['task'] == 'M01.17' and
         disposition['claim'] == claim and disposition['owner_session'] == owner and
         disposition['manifest_sha256'] == F.digest(manifest) and
         disposition['source_binding'] == manifest['source_binding'] and
         disposition['image_id'] == manifest['source_binding']['image_id'] and
         disposition['limits'] == F.LIMITS, 'root-disposition')
    F.sha(disposition['human_message_sha256'])
    F.sha(disposition['review_packet_sha256'])
    need(review_gate['gate'] == 'pass' and review_gate['task_id'] == 'M01.17' and
         review_gate['head'] == manifest['source_binding']['head'] and
         review_gate['tree'] == manifest['source_binding']['tree'] and
         review_gate['packet_sha256'] == disposition['review_packet_sha256'], 'qualified-source-gate')
    need(disposition['archive_identity'] == identity(manifest['payload_plan']['archive_root']), 'root-archive')
    return F.digest(disposition)
