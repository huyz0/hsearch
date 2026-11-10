"""Durable, bounded terminal evidence for the fixed M02.10 research campaign.

A receipt permits disposal only after the source-bound public copy and TaskStore
journal are synced and reread. Failed publication leaves charged orphan bytes.
This controller does not establish native/provider qualification by itself.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import time

MIB = 1024**2
CAP = MIB
EXPORT = 32 * MIB
TASK_RESERVE = 24 * MIB
HOST_RESERVE = 1024 * MIB
HASH = re.compile(r'[a-f0-9]{64}\Z')
NAME = re.compile(r'[a-zA-Z0-9][a-zA-Z0-9_.-]{0,127}\Z')


def need(value, reason):
    if not value:
        raise ValueError(reason)


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode('utf-8')


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def identity(info):
    # Python Windows lstat/fstat disagree on legacy st_ctime after rewrites.
    # POSIX retains change-time proof; Windows HOST fixtures use explicit birth time.
    change = info.st_birthtime_ns if os.name == 'nt' else info.st_ctime_ns
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, change)


def read(path, maximum=CAP):
    path = Path(path)
    need(path.is_absolute() and path.resolve() == path, 'Checkpoint path redirected')
    before = path.lstat()
    need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1 and before.st_size <= maximum,
         'Checkpoint file type/link/size')
    with path.open('rb') as handle:
        need(identity(os.fstat(handle.fileno())) == identity(before), 'Checkpoint replaced on open')
        raw = handle.read(maximum + 1)
        need(len(raw) <= maximum and identity(os.fstat(handle.fileno())) == identity(before),
             'Checkpoint changed during read')
    need(identity(path.lstat()) == identity(before), 'Checkpoint path replaced')
    return raw


def sync_file(path):
    before = path.lstat()
    descriptor = os.open(path, os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0) | getattr(os, 'O_NONBLOCK', 0))
    try:
        need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1 and
             identity(os.fstat(descriptor)) == identity(before), 'Checkpoint sync identity')
        os.fsync(descriptor)
        need(identity(os.fstat(descriptor)) == identity(before) == identity(path.lstat()),
             'Checkpoint changed during sync')
    finally:
        os.close(descriptor)


def sync_directory(path):
    need(os.name == 'posix', 'Campaign durability requires qualified POSIX directory sync')
    need(path.resolve() == path and not path.is_symlink(), 'Checkpoint directory redirected')
    before = path.lstat()
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | getattr(os, 'O_NOFOLLOW', 0))
    try:
        actual = os.fstat(descriptor)
        need(stat.S_ISDIR(actual.st_mode) and (actual.st_dev, actual.st_ino) == (before.st_dev, before.st_ino),
             'Checkpoint directory identity')
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def allowed_export(mode, name):
    if mode == 'durability-object':
        return re.fullmatch(r'index\.json|run\.jsonl|faults\.jsonl|case-(?:0[0-9]{2}|1[0-3][0-9]|14[0-3])\.jsonl', name) is not None
    if mode == 'durability-sample':
        return re.fullmatch(r'index\.json|receipt\.json|stdout\.log|stderr\.log|command\.json|payload-[0-9]{3}\.bin', name) is not None
    return False


def public_bytes(raw):
    need(not any(marker in raw.lower() for marker in (b'authorization:', b'authorization=',
         b'x-amz-credential', b'-----begin private key-----', b'-----begin rsa private key-----')),
         'Secret-bearing campaign output refused before public encoding')


class Deadline:
    def __init__(self, cutoff):
        need(type(cutoff) in (int, float) and 0 < cutoff < float('inf'), 'Campaign deadline required')
        self.cutoff = cutoff

    def require(self, seconds):
        need(0 <= seconds <= self.cutoff - time.monotonic(), 'Campaign incomplete: finalization allowance exhausted')


class Journal:
    def __init__(self, store, claim, owner, source, output, deadline):
        self.store, self.claim, self.owner, self.source = store, claim, owner, source
        self.output, self.deadline = Path(output), Deadline(deadline)
        need(self.output.is_absolute() and self.output.resolve() == self.output and self.output.is_dir(),
             'Fixed external campaign output required')
        need(not self.output.is_relative_to(Path(store.get(claim, owner)['worktree'])), 'Campaign output inside source')
        self.events = []

    def admit_task_reserve(self):
        """No new pool or quota: require room inside the existing TaskStore limits."""
        with self.store.edit() as data:
            row=data['tasks'][self.claim]
            folder=self.store.root/'evidence'/self.claim
            count=total=0
            if folder.exists():
                need(folder.resolve()==folder and not folder.is_symlink(), 'Campaign task evidence root redirected')
                for path in folder.iterdir():
                    need(re.fullmatch(r'[a-f0-9]{64}\.json',path.name), 'Unknown task evidence orphan')
                    info=path.lstat()
                    need(stat.S_ISREG(info.st_mode) and info.st_nlink==1 and info.st_size<=row['packet']['output_budget_bytes'],
                         'Campaign task evidence type/link/size')
                    count+=1;total+=info.st_size
                    need(count<=1000 and total<=64*MIB, 'Campaign task evidence exceeds existing limits')
            need(max(count,len(row['evidence']))+144<=1000 and
                 max(total,sum(item['bytes'] for item in row['evidence']))+TASK_RESERVE<=64*MIB,
                 'Campaign requires24MiB and144 records inside existing task quota; orphans retained')

    def retained(self):
        total = count = 0
        # Both complete and interrupted event directories remain charged.
        for directory in self.output.iterdir():
            need(NAME.fullmatch(directory.name) and directory.is_dir() and not directory.is_symlink(),
                 'Unknown campaign retained directory')
            count += 1
            need(count <= 28, 'Campaign attempt count exhausted')
            files = list(directory.iterdir())
            need(len(files) <= 256, 'Campaign retained file count')
            subtotal = 0
            for path in files:
                need(NAME.fullmatch(path.name), 'Campaign retained filename')
                info = path.lstat()
                need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= CAP,
                     'Campaign retained type/link/size')
                subtotal += info.st_size
                need(subtotal <= EXPORT, 'Campaign event storage bound')
            total += subtotal
            need(total <= HOST_RESERVE // 2, 'Campaign retained aggregate storage bound')
        return total, count

    def write(self, folder, name, raw, used):
        self.deadline.require(0)
        need(NAME.fullmatch(name) and len(raw) <= CAP and used + len(raw) <= EXPORT,
             'Campaign publication byte/name bound')
        # All old/orphan bytes are counted before each allocation.
        total, _ = self.retained()
        need(total + len(raw) <= HOST_RESERVE // 2, 'Campaign publication aggregate bound')
        path = folder / name
        with path.open('xb') as handle:
            handle.write(raw)
            handle.flush()
            os.fsync(handle.fileno())
        need(read(path) == raw, 'Campaign publication reread mismatch')
        return used + len(raw)

    def terminal(self, context, result, export=None):
        self.deadline.require(600)
        started = time.monotonic()
        record = self.store.get(self.claim, self.owner)
        need(record['packet']['task_id'] == 'M02.10' and record['packet'].get('research_campaign') == 'durability-v1',
             'Checkpoint outside fixed campaign packet')
        need(self.store.fingerprint(record) == self.source, 'Campaign checkpoint source changed')
        need(context['claim'] == self.claim and context['container_id'] == result['container_id'] and
             context['source'] == self.source and context['terminal']['running'] is False and
             context['terminal']['exit_code'] == result['exit_code'], 'Campaign terminal binding')
        need(len(encoded(context)) <= 16384, 'Campaign context bound')
        verified = not result['exit_code'] and not result['truncated'] and not result.get('verification_failed')
        if verified:
            need(context.get('mode') in {'prefetch','check','durability-sample','durability-object'} and
                 HASH.fullmatch(context.get('packet_sha256','')) and HASH.fullmatch(context.get('image_inputs_sha256','')) and
                 re.fullmatch(r'sha256:[a-f0-9]{64}', context.get('image','')) and HASH.fullmatch(context.get('config_sha256','')) and
                 re.fullmatch(r'hsearch-[a-z0-9-]{1,25}-[a-f0-9]{12}', context.get('namespace','')) and
                 re.fullmatch(r'hsearch-run-[a-f0-9]{12}', context.get('run_lease','')) and
                 type(context.get('harness_hashes')) is dict and 1 <= len(context['harness_hashes']) <= 16 and
                 all(NAME.fullmatch(name) and HASH.fullmatch(value) for name,value in context['harness_hashes'].items()),
                 'Missing authenticated campaign execution binding')
            expected_budget = {'cpu_millis':2000,'memory_bytes':4*1024**3,'pids':256,
                               'disk_bytes':(18 if context['mode']=='durability-object' else 8)*1024**3}
            need(context.get('budget') == expected_budget and type(context.get('command_budget')) is dict,
                 'Campaign execution envelope changed')
        total, count = self.retained()
        need(count < 28 and len(self.events) < 28, 'Campaign attempt bound')
        # Admission reserves a whole event before its first public file.
        need(total + EXPORT <= HOST_RESERVE // 2, 'Campaign event reserve unavailable')
        payloads = {}
        if export is not None:
            export = Path(export)
            need(export.is_absolute() and export.resolve() == export and export.is_dir(), 'Campaign export redirected')
            for path in export.iterdir():
                need(len(payloads) < 256 and allowed_export(context.get('mode'), path.name), 'Private or unknown campaign export')
                raw = read(path)
                pin = {'bytes': len(raw), 'sha256': digest(raw)}
                need(result.get('export_files', {}).get(path.name) == pin, 'Campaign export changed after oracle')
                public_bytes(raw)
                payloads[path.name] = raw
                need(sum(map(len, payloads.values())) <= EXPORT, 'Campaign export aggregate bound')
        need(set(payloads) == set(result.get('export_files', {})), 'Campaign export inventory changed after oracle')
        streams = result.get('raw_streams', {'stdout': result['output'].encode(), 'stderr': b''})
        need(set(streams) == {'stdout', 'stderr'} and all(type(raw) is bytes for raw in streams.values()) and
             sum(map(len, streams.values())) <= 512 * 1024, 'Campaign exact log byte bound')
        for raw in streams.values():
            public_bytes(raw)
        public_bytes(encoded({k: v for k, v in result.items() if k not in {'raw_streams', 'output'}}))
        key = 'attempt-' + str(count).zfill(2)
        folder = self.output / key
        used, files, pending = 0, {}, {}
        def stage(name, raw, used):
            need(NAME.fullmatch(name) and name not in pending and len(raw)<=CAP and used+len(raw)<=EXPORT,
                 'Campaign complete publication plan exceeds bound')
            pending[name]=raw
            return used+len(raw)
        for name, raw in sorted(payloads.items()):
            used = stage(name, raw, used)
            files[name] = {'bytes': len(raw), 'sha256': digest(raw)}
        logs = {}
        for stream, raw in streams.items():
            chunks = []
            for offset in range(0, len(raw), 192 * 1024):
                name = 'log-' + stream + '-' + str(len(chunks)) + '.json'
                content = encoded({'encoding': 'base64', 'raw': base64.b64encode(raw[offset:offset + 192 * 1024]).decode('ascii')})
                used = stage(name, content, used)
                files[name] = {'bytes': len(content), 'sha256': digest(content)}
                chunks.append(name)
            logs[stream] = {'chunks': chunks, 'bytes': len(raw), 'sha256': digest(raw)}
        proof = {k: v for k, v in result.items() if k not in {'output', 'raw_streams'}}
        proof_raw = encoded(proof)
        used = stage('proof.json', proof_raw, used)
        files['proof.json'] = {'bytes': len(proof_raw), 'sha256': digest(proof_raw)}
        inventory_raw = encoded(files)
        used = stage('inventory.json', inventory_raw, used)
        inventory = {'bytes':len(inventory_raw), 'sha256':digest(inventory_raw)}
        manifest = {'schema': 1, 'context': context, 'inventory': inventory, 'logs': logs,
                    'status': 'verified' if verified else 'failed-unqualified'}
        need(len(files) + 3 <= 256, 'Campaign whole-event file count')
        raw = encoded(manifest)
        need(len(raw) <= 16384, 'Campaign compact manifest bound')
        used = stage('manifest.json', raw, used)
        reference = {'schema': 1, 'event': key, 'manifest_sha256': digest(raw), 'manifest_bytes': len(raw),
                     'container_id': result['container_id'], 'status': manifest['status']}
        need(len(encoded(reference)) <= 16384, 'Campaign reference bound')
        stage('reference.json', encoded(reference), used)
        need(len(pending)<=256, 'Campaign planned file count')
        folder.mkdir(mode=0o700)
        written=0
        for name, content in pending.items():
            written=self.write(folder,name,content,written)
        sync_directory(folder)
        sync_directory(self.output)
        # TaskStore owns quota/orphan accounting and commits the reference under
        # its normal registry lock. Then sync the containing names, not just data.
        self.store.append(self.claim, self.owner, 'campaign-checkpoint', reference)
        with self.store.edit() as data:
            current = data['tasks'][self.claim]
            need(any(item['kind'] == 'campaign-checkpoint' and item['sha256'] == digest(encoded(reference))
                     for item in current['evidence']), 'Campaign reference not in journal')
            evidence = self.store.root / 'evidence' / self.claim
            sync_file(evidence / (digest(encoded(reference)) + '.json'))
            sync_directory(evidence)
            sync_directory(evidence.parent)
        self.verify(reference)
        need(self.store.fingerprint(record)==self.source, 'Campaign source changed during checkpoint')
        sync_file(self.store.path)
        sync_directory(self.store.root)
        sync_directory(self.store.root.parent)
        need(time.monotonic() - started <= 600, 'Campaign checkpoint deadline exceeded; retain worker')
        self.events.append(reference)
        return reference

    def verify(self, reference):
        need(NAME.fullmatch(reference['event']) and HASH.fullmatch(reference['manifest_sha256']), 'Checkpoint reference malformed')
        folder = self.output / reference['event']
        raw = read(folder / 'manifest.json', 16384)
        need(len(raw) == reference['manifest_bytes'] and digest(raw) == reference['manifest_sha256'], 'Checkpoint manifest changed')
        manifest = json.loads(raw)
        need(manifest['context']['claim'] == self.claim and manifest['context']['source'] == self.source and
             manifest['context']['container_id'] == reference['container_id'] and manifest['status'] == reference['status'],
             'Checkpoint manifest binding changed')
        need(read(folder/'reference.json', 16384) == encoded(reference), 'Checkpoint public reference changed')
        inventory_raw = read(folder / 'inventory.json')
        need(len(inventory_raw) == manifest['inventory']['bytes'] and digest(inventory_raw) == manifest['inventory']['sha256'],
             'Checkpoint public inventory binding changed')
        files = json.loads(inventory_raw)
        need(type(files) is dict and len(files) <= 253 and
             {x.name for x in folder.iterdir()} == set(files) | {'manifest.json', 'inventory.json', 'reference.json'}, 'Checkpoint public inventory changed')
        for name, pin in files.items():
            need(NAME.fullmatch(name), 'Checkpoint manifest path')
            content = read(folder / name)
            need(len(content) == pin['bytes'] and digest(content) == pin['sha256'], 'Checkpoint public file changed')
        total = 0
        for stream in ('stdout','stderr'):
            log = manifest['logs'][stream]
            need(type(log['chunks']) is list and len(log['chunks']) <= 3, 'Checkpoint log chunk bound')
            restored = bytearray()
            for name in log['chunks']:
                need(name in files and name.startswith('log-'+stream+'-'), 'Checkpoint log chunk identity')
                chunk = json.loads(read(folder/name))
                need(chunk['encoding'] == 'base64', 'Checkpoint log encoding')
                restored.extend(base64.b64decode(chunk['raw'], validate=True))
                need(len(restored) <= 512*1024, 'Checkpoint decoded log bound')
            total += len(restored)
            need(len(restored) == log['bytes'] and digest(restored) == log['sha256'], 'Checkpoint raw log binding changed')
        need(total <= 512*1024, 'Checkpoint aggregate log bound')
        need(read(self.store.root/'evidence'/self.claim/(digest(encoded(reference))+'.json')) == encoded(reference),
             'Checkpoint journal blob changed')
        registry = json.loads(read(self.store.path, 64 * MIB))
        records = registry['tasks'][self.claim]['evidence']
        need(any(item['kind'] == 'campaign-checkpoint' and item['sha256'] == digest(encoded(reference)) for item in records),
             'Checkpoint journal reference absent')
        return reference
