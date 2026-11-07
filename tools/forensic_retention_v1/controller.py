"""Exclusive pack, durable map/audit, and exact-prefix retirement lifecycle."""
import hashlib
import io
import os
from pathlib import Path
import stat
import sys
import tarfile
import time

from . import protocol as F
from . import _pack as P
from ._bytes import need, regular
from ._sync import sync_directory
from .storage import Archive, Metadata, CheckedWriter, DurableMeter, audit, identity
from .lifecycle import authority


def order(manifest):
    entries = manifest['closure']['entries']
    return sorted((e for e in entries if e['kind'] == 'file'), key=lambda e:e['path']) + sorted(
        (e for e in entries if e['kind'] == 'directory'), key=lambda e:(-e['path'].count('/'), e['path']))


class Controller:
    def __init__(self, archive, metadata, lifecycle, closed_job, backend, *, sync=sync_directory, fault=None):
        need(type(archive) is Archive and archive.host_private is True and type(metadata) is Metadata, 'controller-capability')
        self.archive, self.metadata, self.lifecycle = archive, metadata, lifecycle
        self.closed_job, self.backend = closed_job, backend
        self.sync, self.fault = sync, fault or (lambda _: None)
        self.meter = None
        self.raw_bytes = 0

    def point(self, phase):
        self.meter.check()
        self.fault(phase)

    def finish(self, success):
        primary=sys.exception()
        try:
            self.lifecycle.finish(success)
        except BaseException:
            if primary is None:
                raise
            primary.add_note('forensic-lifecycle-finalization-uncertain')

    def prepare(self, source_binding, preparation, review_gate):
        """Read-only preparation requires an explicit qualified-source root grant."""
        F.keys(preparation, 'kind task claim owner_session source_binding archive_identity')
        need(preparation == dict(kind='root-readonly-preparation-v1', task='M01.17',
             claim=self.lifecycle.claim, owner_session=self.lifecycle.owner,
             source_binding=source_binding, archive_identity=self.archive.identity), 'preparation-authority')
        F.source(source_binding)
        need(review_gate['gate']=='pass' and review_gate['task_id']=='M01.17' and
             review_gate['head']==source_binding['head'] and review_gate['tree']==source_binding['tree'],
             'qualified-preparation-source')
        need(self.lifecycle.source(source_binding['image_id']) == source_binding, 'source-changed')
        self.closed_job.check()
        now = time.time_ns()//1000000
        meter = F.Meter(dict(started_ms=now, deadline_ms=now+1200000))
        rows = self.archive.selected(meter)
        closed = F.closure(rows)
        physical = self.archive.physical(meter)
        # Qualification cannot run in this exclusive lifecycle. Stage within
        # the physical cap, then retain the unchanged 42-node future reserve.
        need(physical['entries'] + 1 <= 4096 and physical['entries']-83+42 <= 4096,
             'qualification-node-reserve')
        result = F.manifest(closed, self.archive.root, physical, source_binding, self.archive.outside(meter))
        F.audit_cost_bound(result['payload_plan']['pack_bytes'], physical['bytes']-224852)
        self.closed_job.check()
        need(self.lifecycle.source(source_binding['image_id']) == source_binding, 'source-changed')
        return result

    def check(self, manifest, pack=False, *, source=False):
        self.meter.check()
        self.archive.check()
        if pack:
            need(identity(self.archive.root/manifest['pack_name'])==self.pack_identity,'pack-identity-changed')
        self.closed_job.check()
        if source:
            self.lifecycle.check(manifest)
        need(self.archive.outside(self.meter, manifest['pack_name'] if pack else None) == manifest['outside_sha256'],
             'outside-changed')

    def stage(self, manifest):
        need(self.archive.selected(self.meter) == manifest['closure']['entries'], 'selected-changed')
        physical = self.archive.physical(self.meter)
        plan = manifest['payload_plan']
        need(physical == dict(entries=plan['original_physical_entries'], bytes=plan['original_physical_bytes']) and
             physical['entries']+1 <= 4096 and physical['entries']-83+42 <= 4096,
             'physical-or-reserve-changed')
        self.metadata.append('journal', dict(phase='exclusive-pack-intent', manifest=F.digest(manifest)))
        self.point('before-exclusive')
        path = self.archive.root/manifest['pack_name']
        with path.open('xb') as stream:
            if os.name!='nt':
                os.fchmod(stream.fileno(),0o644)
            self.point('after-exclusive')
            with tarfile.open(fileobj=CheckedWriter(stream, plan['pack_bytes'], self.meter), mode='w:',
                              format=tarfile.USTAR_FORMAT, encoding='utf-8', errors='strict') as pack:
                for row in manifest['closure']['entries']:
                    self.point('member-write')
                    payload = None
                    if row['kind'] == 'file':
                        self.meter.charge(row['bytes'])
                        raw = regular(self.archive.root/row['path'], 8388608, row['sha256'])
                        need(P.metadata(self.archive.root/row['path']) == {k:v for k,v in row.items() if k not in {'path','sha256'}},
                             'selected-metadata-changed')
                        payload = io.BytesIO(raw)
                    pack.addfile(P.header(row), payload)
                raw = F.enc(plan['index'])
                info = tarfile.TarInfo(P.INDEX_NAME)
                info.size, info.mode, info.mtime = len(raw), 0o600, 0
                pack.addfile(info, io.BytesIO(raw))
            stream.flush()
            self.point('pack-file-sync')
            os.fsync(stream.fileno())
        self.point('pack-directory-sync')
        self.sync(self.archive.root)
        need(path.stat().st_size == plan['pack_bytes'], 'pack-size')
        self.meter.charge(path.stat().st_size)
        pin = regular(path, 2097152, retain=False)['sha256']
        self.pack_identity=identity(path)
        need(self.archive.selected(self.meter) == manifest['closure']['entries'], 'selected-changed')
        return pin

    def run(self, manifest, authorization, admission, disposition, review_gate):
        F.validate(manifest)
        authority(manifest, authorization, disposition, review_gate,
                  owner=self.lifecycle.owner, claim=self.lifecycle.claim)
        need(disposition['metadata_root'] == str(self.metadata.root) and not self.metadata.rows(), 'metadata-operation-binding')
        need(admission == F.admission(manifest, authorization, started_ms=admission['started_ms'],
                                     deadline_ms=admission['deadline_ms']), 'admission-binding')
        need(str(self.archive.root) == manifest['payload_plan']['archive_root'], 'archive-binding')
        self.meter = DurableMeter(admission,self.metadata)
        need(self.lifecycle.source(manifest['source_binding']['image_id'])==manifest['source_binding'],'source-changed')
        self.closed_job.check()
        self.backend.preflight(manifest,self.meter)
        self.meter.check()
        self.lifecycle.begin(manifest, self.metadata.root)
        success = False
        try:
            self.metadata.append('reservation', dict(manifest=F.digest(manifest), disposition=F.digest(disposition),
                                 archive_identity=self.archive.identity, metadata_identity=self.metadata.identity))
            self.metadata.append('manifest', manifest)
            self.metadata.append('authorization', authorization)
            self.metadata.append('admission', admission)
            self.check(manifest)
            pack_sha = self.stage(manifest)
            self.check(manifest, pack=True, source=True)
            self.point('before-worker')
            receipt = self.backend.verify(self.archive, manifest, pack_sha, self.meter, self.metadata)
            raw = F.enc(receipt)
            need(len(raw) <= 8192 and receipt == audit(self.archive, manifest, pack_sha, self.meter), 'worker-integrity')
            self.raw_bytes = max(len(raw),getattr(self.backend,'raw',0))
            need(self.raw_bytes<=8192,'aggregate-raw')
            self.metadata.append('worker', receipt)
            need(self.archive.selected(self.meter)==manifest['closure']['entries'],'post-worker-originals-changed')
            self.point('before-map')
            self.check(manifest,pack=True,source=True)
            mapping = dict(kind='failed-forensic-map-v1', manifest_sha256=F.digest(manifest), pack_sha256=pack_sha,
                           index_sha256=manifest['payload_plan']['index_sha256'], pack_name=manifest['pack_name'],
                           worker_sha256=F.digest(receipt), authorization_sha256=F.digest(authorization),
                           admission_sha256=F.digest(admission), disposition_sha256=F.digest(disposition),
                           archive_identity=self.archive.identity, pack_identity=self.pack_identity,
                           identities={r['path']:identity(self.archive.root/r['path']) for r in order(manifest)},
                           qualification=False, semantic_export=False, original_cli_exit=1)
            self.metadata.append('mapping', mapping)
            self.point('after-map')
            proof = audit(self.archive, manifest, pack_sha, self.meter)
            self.metadata.append('audit', proof)
            self.point('after-audit')
            self.check(manifest, pack=True, source=True)
            result = self.retire(manifest, mapping)
            self.metadata.append('terminal', result)
            success = True
            return result
        finally:
            self.finish(success)

    def prefix(self, manifest, mapping):
        """Find the exact missing prefix, including unlink-before-ack crashes."""
        count = 0
        found_present = False
        for row in order(manifest):
            path = self.archive.root/row['path']
            # lexists treats a newly injected dangling symlink as present.
            if not os.path.lexists(path):
                need(not found_present, 'retirement-not-prefix')
                count += 1
                continue
            found_present = True
            need(identity(path) == mapping['identities'][row['path']], 'retirement-identity')
            actual = P.metadata(path)
            expected = {k:v for k,v in row.items() if k not in {'path','sha256'}}
            if row['kind'] == 'directory':
                # Removing our children changes mtime and nlink, while recorded
                # original values remain losslessly preserved in the map index.
                actual.pop('mtime_ns'); expected.pop('mtime_ns')
                actual.pop('nlink'); expected.pop('nlink')
            else:
                self.meter.charge(row['bytes'])
                regular(path, 8388608, row['sha256'], retain=False)
            need(actual == expected, 'retirement-metadata')
        return count

    def retire(self, manifest, mapping):
        need(self.metadata.one('mapping') == mapping and self.metadata.one('audit') ==
             audit(self.archive, manifest, mapping['pack_sha256'], self.meter), 'durable-map-audit')
        self.metadata.reaffirm('mapping')
        self.metadata.reaffirm('audit')
        self.point('before-retire-intent')
        journals = [r['value'] for r in self.metadata.rows() if r['kind'] == 'journal']
        intent = dict(phase='retire-intent', mapping_sha256=F.digest(mapping), paths=[r['path'] for r in order(manifest)])
        existing = [r for r in journals if r['phase'] == 'retire-intent']
        need(not existing or existing == [intent], 'retirement-intent-changed')
        if not existing:
            self.metadata.append('journal', intent)
        prefix = self.prefix(manifest, mapping)
        self.check(manifest, pack=True)
        for i, row in enumerate(order(manifest)[prefix:], prefix):
            self.meter.check()
            path = self.archive.root/row['path']
            need(identity(path) == mapping['identities'][row['path']], 'retirement-identity')
            if row['kind']=='file':
                self.meter.charge(row['bytes'])
                regular(path,8388608,row['sha256'],retain=False)
            self.point('before-unlink')
            if row['kind'] == 'file':
                path.unlink()
            else:
                path.rmdir()
            self.point('after-unlink')
            self.sync(path.parent)
            self.point('after-unlink-sync')
            if (i+1) % 8 == 0 or i == 83:
                self.metadata.append('journal', dict(phase='retired-prefix', count=i+1, mapping_sha256=F.digest(mapping)))
        need(self.prefix(manifest, mapping) == 84, 'retirement-incomplete')
        proof = audit(self.archive, manifest, mapping['pack_sha256'], self.meter)
        self.check(manifest, pack=True, source=True)
        physical = self.archive.physical(self.meter)
        plan = manifest['payload_plan']
        need(physical == dict(entries=plan['final_physical_entries'], bytes=plan['final_physical_bytes']), 'final-accounting')
        result = dict(kind='failed-forensic-result-v1', manifest_sha256=F.digest(manifest), mapping_sha256=F.digest(mapping),
                      integrity_sha256=F.digest(proof), originals_retired=84, qualification=False, semantic_export=False,
                      original_cli_exit=1, guest_tests=601, original_host_proof=False, cause='unconfirmed',
                      audit_bytes=self.meter.used, raw_bytes=self.raw_bytes, physical=physical)
        need(len(F.enc(result)) <= 16384, 'public-output')
        return result

    def recover(self, disposition, review_gate):
        manifest = self.metadata.one('manifest')
        authorization = self.metadata.one('authorization')
        admission = self.metadata.one('admission')
        authority(manifest, authorization, disposition, review_gate,
                  owner=self.lifecycle.owner, claim=self.lifecycle.claim)
        need(disposition['metadata_root'] == str(self.metadata.root), 'recovery-metadata-root')
        self.meter = DurableMeter(admission,self.metadata)  # Original deadline/pool, never extended.
        self.closed_job.check()
        self.backend.preflight(manifest,self.meter)
        self.meter.check()
        mapping = self.metadata.one('mapping')
        self.pack_identity=mapping['pack_identity']
        need(identity(self.archive.root/manifest['pack_name'])==self.pack_identity,'recovery-pack-identity')
        need(mapping['manifest_sha256'] == F.digest(manifest) and
             mapping['authorization_sha256'] == F.digest(authorization) and
             mapping['admission_sha256'] == F.digest(admission) and
             mapping['disposition_sha256'] == F.digest(disposition) and
             mapping['archive_identity'] == self.archive.identity, 'recovery-provenance')
        reservation = self.metadata.one('reservation')
        need(reservation['metadata_identity'] == self.metadata.identity and
             reservation['archive_identity'] == self.archive.identity, 'recovery-root-identity')
        need(not any(r['kind'] == 'terminal' for r in self.metadata.rows()), 'operation-already-terminal')
        self.lifecycle.recover(manifest, self.metadata.root)
        success = False
        try:
            result = self.retire(manifest, mapping)
            self.metadata.append('terminal', result)
            success = True
            return result
        finally:
            self.finish(success)


class ForensicArchive:
    """Composite retained consumer. Opaque access never produces a job Reader."""
    def __init__(self, archive, metadata, meter, *, disposition, review_gate, claim, owner):
        self.archive, self.metadata, self.meter = archive, metadata, meter
        self.manifest = metadata.one('manifest')
        F.validate(self.manifest)
        authority(self.manifest,metadata.one('authorization'),disposition,review_gate,claim=claim,owner=owner)
        self.mapping = metadata.one('mapping')
        F.keys(self.mapping,'kind manifest_sha256 pack_sha256 index_sha256 pack_name worker_sha256 authorization_sha256 admission_sha256 disposition_sha256 archive_identity pack_identity identities qualification semantic_export original_cli_exit')
        need(self.mapping['kind']=='failed-forensic-map-v1' and self.mapping['qualification'] is False and
             self.mapping['semantic_export'] is False and self.mapping['original_cli_exit']==1 and
             self.mapping['pack_name']==self.manifest['pack_name'] and
             self.mapping['index_sha256']==self.manifest['payload_plan']['index_sha256'] and
             self.mapping['disposition_sha256']==F.digest(disposition) and
             self.mapping['worker_sha256']==F.digest(metadata.one('worker')) and
             self.mapping['pack_identity']==identity(archive.root/self.manifest['pack_name']) and
             set(self.mapping['identities'])=={r['path'] for r in self.manifest['closure']['entries']},'consumer-map-schema')
        need(self.mapping['manifest_sha256'] == F.digest(self.manifest) and
             self.mapping['archive_identity'] == archive.identity and
             metadata.one('authorization') == F.authorization(self.manifest, metadata.one('authorization')['answer']) and
             self.mapping['authorization_sha256'] == F.digest(metadata.one('authorization')) and
             self.mapping['admission_sha256'] == F.digest(metadata.one('admission')), 'consumer-provenance')
        need(metadata.one('audit') == audit(archive, self.manifest, self.mapping['pack_sha256'], meter), 'consumer-audit')
        reservation=metadata.one('reservation')
        need(reservation['metadata_identity']==metadata.identity and reservation['archive_identity']==archive.identity and
             reservation['disposition']==F.digest(disposition),'consumer-reservation')

    def totals(self):
        expected={r['path']:r for r in self.manifest['closure']['entries']}
        groups=[g for g in F.PAIR if os.path.lexists(self.archive.root/g)]
        present=self.archive.scan(groups,meter=self.meter)
        for row in present:
            need(row['path'] in expected and identity(self.archive.root/row['path'])==self.mapping['identities'][row['path']],
                 'consumer-original-alias')
            original=dict(expected[row['path']])
            current=dict(row)
            if row['kind']=='directory':
                for key in ['mtime_ns','nlink']:
                    original.pop(key);current.pop(key)
            need(current==original,'consumer-original-changed')
        need(self.archive.outside(self.meter,self.manifest['pack_name'])==self.manifest['outside_sha256'],'consumer-outside-changed')
        physical = self.archive.physical(self.meter)
        return F.composite(physical, self.manifest, dict(entries=len(present), bytes=sum(r['bytes'] for r in present)))

    def member(self, name, expected_sha256):
        pack = self.open()
        try:
            return pack.member(name, expected_sha256)
        except BaseException:
            pack.close()
            raise

    def open(self):
        """One verified session supports all 69 members within the audit pool."""
        return P.VerifiedPack(self.archive.root/self.manifest['pack_name'], self.manifest['payload_plan'],
                             F.digest(self.manifest['payload_plan']), self.mapping['pack_sha256'], audit=F.audit_for(self.meter))

    semantic_reader = staticmethod(F.semantic_reader)
