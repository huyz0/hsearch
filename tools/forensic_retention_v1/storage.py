"""Bounded filesystem capabilities and append-only durable operation metadata."""
import hashlib
import os
from pathlib import Path
import stat
import time

from . import _pack as P
from ._bytes import Refused, need, regular
from ._sync import sync_directory
from . import protocol as F


def identity(path):
    path = Path(path).absolute()
    need(path.resolve() == path and not path.is_symlink(), 'identity-redirect')
    s = path.lstat()
    need(not getattr(s, 'st_file_attributes', 0) & 1024, 'identity-reparse')
    return [s.st_dev, s.st_ino, stat.S_IFMT(s.st_mode)]


def private_ancestor(path):
    """A file bind may be readable in the guest while its host tree stays private."""
    if os.name=='nt':
        # POSIX mode bits do not describe Windows ACLs. Windows durability is
        # separately unqualified and strict directory sync still fails closed.
        return
    path=Path(path).absolute()
    need(path.resolve()==path,'private-path-redirect')
    candidates=[path,*path.parents]
    need(len(candidates)<=64,'private-path-depth')
    need(any(p.stat().st_uid==os.getuid() and stat.S_IMODE(p.stat().st_mode)&0o077==0
             for p in candidates),'private-host-ancestor-required')


def decode(raw, maximum=1048576, *, canonical=True):
    # Reject deep JSON before allocating the object graph.
    import json
    need(type(raw) is bytes and len(raw) <= maximum, 'metadata-input-bound')
    depth = 0
    quoted = escaped = False
    for c in raw:
        if quoted:
            if escaped:
                escaped = False
            elif c == 92:
                escaped = True
            elif c == 34:
                quoted = False
        elif c == 34:
            quoted = True
        elif c in (91, 123):
            depth += 1
            need(depth <= 24, 'metadata-depth')
        elif c in (93, 125):
            depth -= 1
            need(depth >= 0, 'metadata-depth')
    need(depth == 0 and not quoted, 'metadata-format')
    def pairs(rows):
        out = {}
        for k, v in rows:
            need(k not in out, 'metadata-duplicate')
            out[k] = v
        return out
    try:
        value = json.loads(raw, object_pairs_hook=pairs,
                           parse_constant=lambda _: need(False, 'metadata-number'))
        P.bounded(value)
        need(not canonical or F.enc(value) == raw, 'metadata-noncanonical')
        return value
    except (UnicodeError, RecursionError, json.JSONDecodeError):
        raise Refused('metadata-format') from None


class Metadata:
    """One bounded append-only stream. Uncertain writes are never overwritten.

    Each record is exclusive-created and both file and directory are synced.
    An incomplete orphan stops loading and remains charged. There is no reset,
    truncation, automatic retry, or garbage collection API.
    """
    MAX_BYTES = 14417920

    def __init__(self, root, *, sync=sync_directory, fault=None):
        self.root = Path(root).absolute()
        need(self.root.resolve() == self.root, 'metadata-root')
        private_ancestor(self.root)
        self.sync = sync
        self.fault = fault or (lambda _: None)
        self.identity = identity(self.root)

    def rows(self):
        need(identity(self.root) == self.identity, 'metadata-root-changed')
        private_ancestor(self.root)
        paths = P.list_bounded(self.root, 96)
        total = 0
        rows = []
        previous = None
        journal = 0
        for i, path in enumerate(sorted(paths)):
            need(path.name == '%03d.json' % i, 'metadata-orphan')
            size = path.lstat().st_size
            total += size
            need(total <= self.MAX_BYTES, 'metadata-total')
            raw = regular(path, 1048576)
            row = decode(raw)
            F.keys(row, 'sequence previous kind value')
            need(row['sequence'] == i and type(row['sequence']) is int and row['previous'] == previous,
                 'metadata-chain')
            need(row['kind'] in {'manifest', 'admission', 'authorization', 'mapping', 'audit',
                                 'journal', 'terminal', 'worker', 'reservation'}, 'metadata-kind')
            need(len(raw) <= (1048576 if row['kind'] == 'manifest' else 131072), 'metadata-record')
            journal += row['kind'] == 'journal'
            need(journal <= 72, 'metadata-journal')
            previous = hashlib.sha256(raw).hexdigest()
            rows.append(row)
        return rows

    def append(self, kind, value):
        rows = self.rows()
        need(kind in {'manifest','admission','authorization','mapping','audit','journal','terminal','worker','reservation'},'metadata-kind')
        previous = F.digest(rows[-1]) if rows else None
        row = dict(sequence=len(rows), previous=previous, kind=kind, value=value)
        import json
        chunks=[]
        size=0
        for chunk in json.JSONEncoder(sort_keys=True,separators=(',',':'),ensure_ascii=True,allow_nan=False).iterencode(row):
            encoded=chunk.encode()
            size+=len(encoded)
            need(size<=1048576,'metadata-encode-bound')
            chunks.append(encoded)
        raw=b''.join(chunks)
        need(len(rows) < 96 and len(raw) <= (1048576 if kind == 'manifest' else 131072), 'metadata-record')
        need(sum(len(F.enc(r)) for r in rows) + len(raw) <= self.MAX_BYTES, 'metadata-total')
        need(kind != 'journal' or sum(r['kind'] == 'journal' for r in rows) < 72, 'metadata-journal')
        path = self.root / ('%03d.json' % len(rows))
        self.fault('metadata-before-open')
        with path.open('xb') as stream:
            if os.name!='nt':
                os.fchmod(stream.fileno(),0o644)
            self.fault('metadata-before-write')
            need(stream.write(raw) == len(raw), 'metadata-short-write')
            stream.flush()
            self.fault('metadata-file-sync')
            os.fsync(stream.fileno())
        self.fault('metadata-directory-sync')
        self.sync(self.root)
        self.fault('metadata-published')
        return hashlib.sha256(raw).hexdigest()

    def path_for(self,kind):
        rows=[r for r in self.rows() if r['kind']==kind]
        need(len(rows)==1,'metadata-cardinality')
        return self.root/('%03d.json'%rows[0]['sequence'])

    def one(self, kind):
        found = [r['value'] for r in self.rows() if r['kind'] == kind]
        need(len(found) == 1, 'metadata-cardinality')
        return found[0]

    def reaffirm(self, kind):
        """Re-establish actual durability after a missing fsync acknowledgement."""
        rows=self.rows()
        found=[r for r in rows if r['kind']==kind]
        need(len(found)==1,'metadata-cardinality')
        row=found[0]
        path=self.root/('%03d.json'%row['sequence'])
        need(regular(path,1048576)==F.enc(row),'metadata-reaffirm-binding')
        with path.open('r+b') as stream:
            os.fsync(stream.fileno())
        self.sync(self.root)


class Archive:
    """Actual bounded archive capability, independent of synthetic fixture types."""
    def __init__(self, root, *, host_private=True):
        self.root = Path(root).absolute()
        need(self.root.is_dir() and self.root.resolve() == self.root, 'archive-root')
        self.identity = identity(self.root)
        self.host_private=host_private

    def check(self):
        need(identity(self.root) == self.identity, 'archive-root-changed')
        if self.host_private:
            private_ancestor(self.root)

    def scan(self, groups=None, *, meter, hash_files=True):
        self.check()
        meter.check()
        pending = [self.root / g for g in groups] if groups is not None else P.list_bounded(self.root, 4096)
        out = []
        total = 0
        seen = set()
        while pending:
            meter.check()
            need(len(out) + len(pending) <= 4096, 'archive-nodes')
            path = pending.pop()
            row = P.metadata(path)
            name = P.logical(path.relative_to(self.root).as_posix())
            need(name.casefold() not in seen, 'archive-alias')
            seen.add(name.casefold())
            row['sha256'] = None
            if row['kind'] == 'directory':
                pending.extend(P.list_bounded(path, 4096-len(out)-len(pending)-1))
            else:
                total += row['bytes']
                need(total <= 67108864 and row['bytes'] <= 8388608, 'archive-bytes')
                if hash_files:
                    meter.charge(row['bytes'])
                    checked = regular(path, 8388608, retain=False)
                    need(P.metadata(path) == {k:v for k,v in row.items() if k != 'sha256'}, 'archive-changed')
                    row['sha256'] = checked['sha256']
            out.append(dict(path=name, **row))
        self.check()
        meter.charge(0, rescan=True)
        return sorted(out, key=lambda r:r['path'])

    def selected(self, meter):
        return self.scan(F.PAIR, meter=meter)

    def outside(self, meter, pack_name=None):
        self.check()
        children = P.list_bounded(self.root, 4096)
        need(all(not p.name.startswith('failed-forensic-fff-') or p.name == pack_name for p in children),
             'orphan-pack')
        groups = [p.name for p in children if p.name not in F.PAIR and p.name != pack_name]
        return F.digest(self.scan(groups, meter=meter))

    def physical(self, meter):
        rows = self.scan(meter=meter, hash_files=False)
        return dict(entries=len(rows), bytes=sum(r['bytes'] for r in rows))


class DurableMeter(F.Meter):
    """Reserve I/O before effects; crashes lose unused reservations, never debt.

    Eight MiB byte blocks and individual rescan slots are durably charged into
    the same bounded journal. Recovery uses the original deadline and starts
    from the last reserved ceiling. Metadata exhaustion refuses further I/O.
    """
    def __init__(self, admission, metadata):
        super().__init__(admission)
        self.metadata = metadata
        checkpoints = [r['value'] for r in metadata.rows() if r['kind']=='journal' and
                       r['value'].get('phase')=='io-reservation']
        self.reserved = self.used = self.rescans = 0
        for row in checkpoints:
            F.keys(row, 'phase bytes rescans')
            F.integer(row['bytes'], self.reserved, 268435456)
            F.integer(row['rescans'], self.rescans, 32)
            self.reserved = self.used = row['bytes']
            self.rescans = row['rescans']

    def charge(self, n, *, rescan=False):
        self.check()
        F.integer(n,0,268435456)
        needed = self.used+n
        reserved = max(self.reserved, ((needed+8388607)//8388608)*8388608)
        scans = self.rescans+int(rescan)
        if reserved>268435456 or scans>32:
            self.denied=True
            raise Refused('audit-budget')
        if reserved != self.reserved or rescan:
            self.metadata.append('journal', dict(phase='io-reservation',bytes=reserved,rescans=scans))
            self.reserved = reserved
        self.used = needed
        self.rescans = scans


class CheckedWriter:
    def __init__(self, stream, maximum, meter):
        self.stream, self.maximum, self.meter = stream, maximum, meter

    def write(self, raw):
        self.meter.check()
        need(self.tell() + len(raw) <= self.maximum, 'pack-growth')
        count = self.stream.write(raw)
        need(count == len(raw), 'pack-short-write')
        return count

    def tell(self):
        return self.stream.tell()

    def flush(self):
        self.stream.flush()


def audit(archive, manifest, pack_sha, meter):
    """All-member and bounded-range audit, never successful original-job proof."""
    F.validate(manifest)
    archive.check()
    plan = manifest['payload_plan']
    count = total = 0
    with P.VerifiedPack(archive.root/manifest['pack_name'], plan, F.digest(plan), pack_sha,
                        audit=F.audit_for(meter)) as pack:
        for row in plan['index']['entries']:
            if row['kind'] != 'file':
                continue
            view = pack.member(row['path'], row['sha256'])
            hasher = hashlib.sha256()
            offset = 0
            while offset < view.size:
                raw = view.read_exact_at(offset, min(65536, view.size-offset))
                hasher.update(raw)
                offset += len(raw)
            need(hasher.hexdigest() == row['sha256'], 'audit-member')
            count += 1
            total += offset
    need(count == 69 and total == 224852, 'audit-denominator')
    return dict(kind='failed-forensic-actual-audit-v1', manifest_sha256=F.digest(manifest),
                pack_sha256=pack_sha, files=count, payload_bytes=total,
                qualification=False, semantic_export=False)
