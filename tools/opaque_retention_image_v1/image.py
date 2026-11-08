"""Exact-scope opaque images through admitted ports. No host or operation authority."""
from dataclasses import asdict, dataclass
import hashlib
from pathlib import PurePosixPath
import tarfile
import threading
import unicodedata
from typing import Protocol

from tools.retention_context_v1.codec import Refused, decode, digest, encode, fields, integer, need, pin
from tools.retention_context_v1.context import Original, Session

PAIR = ('hsearch-agent-fixture-3686832deb7c', 'sealed-export-qual3686832deb7c')
NODES, FILES, PAYLOAD = 84, 69, 224852
PACK, INDEX, RANGE = 2097152, 1048576, 65536
ARCHIVE_NODES, ARCHIVE_BYTES, RESERVE = 4096, 67108864, 42
RECORD = 131072
INDEX_NAME = '__hsearch_opaque_image_v1__/index.json'
# Historical data, not an approval, semantic result, or current qualification.
HISTORY = encode(dict(original_cli_exit=1, guest_tests=601, original_host_proof=False,
               cause='unconfirmed', qualification=False, semantic_export=False,
               original_source_head='fffbe1142717f45d538b2979021719db7f09d302',
               original_source_tree='86c6636f934d1648747a5d9d7849c2b776f48740',
               original_handoff_sha256='1b0d0e6bcd32f9919955714ad10e2fb4b5098bab0cbd60b7500de91ce1d4e0d8',
               original_cli_log_sha256='6d810a135da56b72f50db55d830cd8d88ed905ef71e573197e8aa7bef1990457'))
_VIEW_TOKEN = object()


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def align(n, boundary=512):
    return (n + boundary - 1) // boundary * boundary


def logical(name):
    need(type(name) is str and 0 < len(name) <= 512, 'image-path')
    need(unicodedata.normalize('NFC', name) == name and
         not any(ord(c) < 32 or ord(c) == 127 or c in '\\:"<>|?*' for c in name), 'image-path')
    parts = name.split('/')
    need(PurePosixPath(name).as_posix() == name and parts[0] in PAIR and
         all(part not in ('', '.', '..') and not part.endswith(('.', ' ')) for part in parts), 'image-path')
    return name


@dataclass(frozen=True)
class Entry:
    path: str
    kind: str
    mode: int
    uid: int
    gid: int
    mtime_ns: int
    windows_attributes: int
    nlink: int
    bytes: int
    sha256: str | None

    def validate(self):
        logical(self.path)
        need(self.kind in ('file', 'directory'), 'image-kind')
        integer(self.mode, 0, 0o7777)
        for n in (self.uid, self.gid, self.windows_attributes):
            integer(n, 0, 2**32-1)
        integer(self.mtime_ns, -(2**63))
        integer(self.nlink, 1, 2**32-1)
        integer(self.bytes, 0, 8388608)
        need(not self.windows_attributes & 1024, 'image-reparse')
        if self.kind == 'file':
            need(self.nlink == 1, 'image-link')
            pin(self.sha256)
        else:
            need(self.bytes == 0 and self.sha256 is None, 'image-directory-data')
        header(self)


def header(entry):
    info = tarfile.TarInfo(entry.path)
    info.type = tarfile.DIRTYPE if entry.kind == 'directory' else tarfile.REGTYPE
    info.size, info.mode, info.uid, info.gid = entry.bytes, entry.mode, entry.uid, entry.gid
    info.mtime = entry.mtime_ns // 1000000000
    try:
        return info.tobuf(format=tarfile.USTAR_FORMAT, encoding='utf-8', errors='strict')
    except (ValueError, UnicodeError, OverflowError):
        raise Refused('image-ustar-representation') from None


def index_header(length):
    return header(Entry(INDEX_NAME, 'file', 0o444, 0, 0, 0, 0, 1, length, '0'*64))


@dataclass(frozen=True)
class Physical:
    nodes: int
    bytes: int

    def validate(self):
        integer(self.nodes, 0, ARCHIVE_NODES)
        integer(self.bytes, 0, ARCHIVE_BYTES)


@dataclass(frozen=True)
class Plan:
    """Reproducible representation data. Does not authorize its construction."""
    manifest: bytes
    manifest_pin: str
    original: Original
    physical: Physical
    entries: tuple[Entry, ...]
    index: bytes
    pack_bytes: int

    @property
    def context_pin(self):
        return digest(asdict(self.original))

    @property
    def final(self):
        return Physical(self.physical.nodes-NODES+1, self.physical.bytes-PAYLOAD+self.pack_bytes)


def plan(manifest, expected_pin, original, physical):
    """Parse a caller-pinned exact inventory; no discovery, clock, or extra pool."""
    pin(expected_pin)
    need(type(manifest) is bytes and 0 < len(manifest) <= INDEX, 'image-manifest-bound')
    need(sha(manifest) == expected_pin, 'image-manifest-pin')
    value = decode(manifest)
    fields(value, 'schema version history entries')
    need(value['schema'] == 'hsearch-opaque-input-v1' and type(value['version']) is int and
         value['version'] == 1 and encode(value['history']) == HISTORY, 'image-input-version-history')
    need(type(value['entries']) is list and len(value['entries']) == NODES, 'image-node-count')
    entries, aliases = [], set()
    for row in value['entries']:
        fields(row, 'path kind mode uid gid mtime_ns windows_attributes nlink bytes sha256')
        entry = Entry(**row)
        entry.validate()
        alias = entry.path.casefold()
        need(alias not in aliases, 'image-alias')
        aliases.add(alias)
        entries.append(entry)
    need([e.path for e in entries] == sorted(e.path for e in entries), 'image-order')
    by_name = {e.path: e for e in entries}
    need(all(root in by_name and by_name[root].kind == 'directory' for root in PAIR), 'image-roots')
    for entry in entries:
        parts = entry.path.split('/')
        need(all('/'.join(parts[:i]) in by_name and by_name['/'.join(parts[:i])].kind == 'directory'
                 for i in range(1, len(parts))), 'image-parent-closure')
    need(sum(e.kind == 'file' for e in entries) == FILES and sum(e.bytes for e in entries) == PAYLOAD,
         'image-payload-count')
    need(type(original) is Original and type(physical) is Physical, 'image-binding-type')
    original.validate()
    physical.validate()
    need(physical.nodes >= NODES and physical.bytes >= PAYLOAD, 'image-physical-baseline')
    index = encode(dict(schema='hsearch-opaque-image-v1', version=1, manifest_sha256=expected_pin,
                        original=asdict(original), context_sha256=digest(asdict(original)),
                        history=value['history'], entries=value['entries']))
    need(len(index) <= INDEX, 'image-index-bound')
    size = align(sum(512+align(e.bytes) for e in entries) + 512+align(len(index))+1024, tarfile.RECORDSIZE)
    need(size <= PACK and physical.nodes+1 <= ARCHIVE_NODES and physical.bytes+size <= ARCHIVE_BYTES,
         'image-staging-capacity')
    result = Plan(manifest, expected_pin, original, physical, tuple(entries), index, size)
    need(result.final.nodes+RESERVE <= ARCHIVE_NODES, 'image-qualification-reserve')
    result.final.validate()
    return result


def validate_plan(value):
    need(type(value) is Plan, 'image-plan-type')
    need(plan(value.manifest, value.manifest_pin, value.original, value.physical) == value, 'image-plan-binding')


def composite(value, physical, present):
    """Opaque logical accounting while originals remain; no retirement effect."""
    validate_plan(value)
    need(type(physical) is Physical and type(present) is tuple and len(present) <= NODES, 'image-composite-type')
    physical.validate()
    need(all(type(p) is str for p in present) and len(set(present)) == len(present), 'image-present-alias')
    by_name = {e.path: e for e in value.entries}
    need(all(p in by_name for p in present), 'image-present-scope')
    retained = sum(by_name[p].bytes for p in present)
    need(physical.nodes >= 1+len(present) and physical.bytes >= value.pack_bytes+retained, 'image-present-physical')
    result = Physical(physical.nodes+NODES-1-len(present), physical.bytes+PAYLOAD-value.pack_bytes-retained)
    result.validate()
    return result


@dataclass(frozen=True)
class SourceState:
    entry: Entry
    identity: str


class SourcePort(Protocol):
    """Already admitted closed source. stat/read never follow aliases or discover roots.

    The integration binds each opaque identity to an owned stable descriptor and
    checks the metadata/host assumptions. Python protocol shape grants no trust.
    """
    def stat(self, name: str) -> SourceState: ...
    # Reads return at most length bytes. Only the explicit one-byte EOF probe
    # accepts an empty result; every payload read requires its exact length.
    def read_exact_at(self, name: str, offset: int, length: int) -> bytes: ...


@dataclass(frozen=True)
class Blob:
    identity: str
    revision: str
    bytes: int

    def validate(self):
        pin(self.identity)
        pin(self.revision)
        integer(self.bytes, 0, PACK)


class ImagePort(Protocol):
    """Exclusive admitted destination or stable read capability, not a path.

    describe must change revision on every content change, preserve object
    identity, and bind real bounded descriptor reads. sync acknowledges durable
    content for the declared host profile only. Adapters belong to M01.21.
    """
    def describe(self) -> Blob: ...
    def read_exact_at(self, offset: int, length: int) -> bytes: ...
    def write_exact_at(self, offset: int, data: bytes) -> int: ...
    def sync(self) -> bool: ...


@dataclass(frozen=True)
class Evidence:
    manifest_sha256: str
    context_sha256: str
    index_sha256: str
    pack_sha256: str
    pack_bytes: int
    nodes: int = NODES
    files: int = FILES
    payload_bytes: int = PAYLOAD
    opaque_only: bool = True
    qualification: bool = False


class Operation:
    """One build or open. Partial/late/unknown effects deny the shared Session.

    All port work is charged before use through the original M19 durable audit
    credits, then checked afterward. No local clock, allowance or retry exists.
    """
    def __init__(self, value, session):
        validate_plan(value)
        need(type(session) is Session and session.expected_pin == value.context_pin, 'image-original-binding')
        self.plan, self.session, self.used, self.failed = value, session, False, False
        self._lock = threading.Lock()
        session.check()

    def _guard(self, action):
        need(self._lock.acquire(blocking=False), 'image-concurrent')
        try:
            need(not self.failed, 'image-sticky')
            return action()
        except BaseException as error:
            self.failed = True
            try:
                self.session.deny()
            except Exception:
                pass  # Preserve the primary refusal; uncertainty never refunds credits.
            if isinstance(error, (Refused, KeyboardInterrupt, SystemExit)):
                raise
            raise Refused('image-port-uncertain') from None
        finally:
            self._lock.release()

    def _call(self, count, action):
        self.session.reserve(audit_bytes=count)
        try:
            result = action()
        except Exception:
            # Port exception strings may contain source names or payloads.
            raise Refused('image-port-uncertain') from None
        self.session.check()
        return result

    def _describe(self, port):
        result = self._call(RECORD, port.describe)
        need(type(result) is Blob, 'image-blob-type')
        result.validate()
        return result

    def _stat(self, source, entry):
        state = self._call(RECORD, lambda: source.stat(entry.path))
        need(type(state) is SourceState and type(state.entry) is Entry and state.entry == entry, 'image-source-metadata')
        pin(state.identity)
        return state

    def _read(self, port, stamp, offset, length):
        integer(offset, 0, stamp.bytes)
        integer(length, 0, RANGE)
        need(offset+length <= stamp.bytes and self._describe(port) == stamp, 'image-extent-stamp')
        raw = self._call(length, lambda: port.read_exact_at(offset, length))
        need(type(raw) is bytes and len(raw) == length and self._describe(port) == stamp, 'image-read-uncertain')
        return raw

    def _begin(self):
        need(not self.used, 'image-operation-used')
        self.used = True
        self.session.check()

    def build(self, source, destination):
        return self._guard(lambda: self._build(source, destination))

    def _build(self, source, destination):
        self._begin()
        # Conservatively predebit all four metadata/identity passes, including
        # the per-member before/after passes, even if construction fails early.
        self.session.reserve(rescans=4)
        states = [self._stat(source, e) for e in self.plan.entries]
        need(len({s.identity for s in states}) == NODES, 'image-source-identity-alias')
        initial = self._describe(destination)
        need(initial.bytes == 0 and initial.identity not in {s.identity for s in states}, 'image-destination-alias')
        position, hasher = 0, hashlib.sha256()

        def emit(raw):
            nonlocal position
            need(type(raw) is bytes and len(raw) <= RANGE and position+len(raw) <= self.plan.pack_bytes, 'image-write-bound')
            before = self._describe(destination)
            need(before.identity == initial.identity and before.bytes == position, 'image-write-position')
            count = self._call(len(raw), lambda: destination.write_exact_at(position, raw))
            need(type(count) is int and count == len(raw), 'image-partial-write')
            after = self._describe(destination)
            need(after.identity == initial.identity and after.bytes == position+len(raw), 'image-write-uncertain')
            hasher.update(raw)
            position += len(raw)

        def zeros(count):
            while count:
                size = min(count, RANGE)
                emit(bytes(size))
                count -= size

        for entry, state in zip(self.plan.entries, states):
            need(self._stat(source, entry) == state, 'image-source-changed')
            emit(header(entry))
            offset, member_hash = 0, hashlib.sha256()
            while offset < entry.bytes:
                size = min(RANGE, entry.bytes-offset)
                raw = self._call(size, lambda: source.read_exact_at(entry.path, offset, size))
                need(type(raw) is bytes and len(raw) == size, 'image-source-short')
                member_hash.update(raw)
                emit(raw)
                offset += size
            if entry.kind == 'file':
                need(member_hash.hexdigest() == entry.sha256, 'image-source-hash')
                eof = self._call(1, lambda: source.read_exact_at(entry.path, entry.bytes, 1))
                need(type(eof) is bytes and eof == b'', 'image-source-eof')
            zeros(align(entry.bytes)-entry.bytes)
            need(self._stat(source, entry) == state, 'image-source-changed')
        emit(index_header(len(self.plan.index)))
        for offset in range(0, len(self.plan.index), RANGE):
            emit(self.plan.index[offset:offset+RANGE])
        zeros(self.plan.pack_bytes-position)
        need(self._call(0, destination.sync) is True, 'image-sync-unacknowledged')
        for entry, state in zip(self.plan.entries, states):
            need(self._stat(source, entry) == state, 'image-source-changed')
        # Read every header, byte, padding and embedded metadata after sync.
        return self._verify(destination, hasher.hexdigest())

    def open(self, port, expected_pack_sha256):
        def action():
            self._begin()
            return self._verify(port, expected_pack_sha256)
        return self._guard(action)

    def _verify(self, port, expected_pin):
        pin(expected_pin)
        self.session.reserve(rescans=1)
        stamp = self._describe(port)
        need(stamp.bytes == self.plan.pack_bytes, 'image-pack-size')
        position, hasher, offsets = 0, hashlib.sha256(), {}

        def read(size):
            nonlocal position
            raw = self._read(port, stamp, position, size)
            position += size
            hasher.update(raw)
            return raw

        def zeros(count):
            while count:
                size = min(count, RANGE)
                need(not any(read(size)), 'image-padding')
                count -= size

        for entry in self.plan.entries:
            need(read(512) == header(entry), 'image-member-header')
            offsets[entry.path] = position
            left, member_hash = entry.bytes, hashlib.sha256()
            while left:
                size = min(left, RANGE)
                member_hash.update(read(size))
                left -= size
            if entry.kind == 'file':
                need(member_hash.hexdigest() == entry.sha256, 'image-member-hash')
            zeros(align(entry.bytes)-entry.bytes)
        need(read(512) == index_header(len(self.plan.index)), 'image-index-header')
        for offset in range(0, len(self.plan.index), RANGE):
            need(read(min(RANGE, len(self.plan.index)-offset)) == self.plan.index[offset:offset+RANGE], 'image-index')
        zeros(self.plan.pack_bytes-position)
        eof = self._call(1, lambda: port.read_exact_at(stamp.bytes, 1))
        need(type(eof) is bytes and eof == b'', 'image-pack-eof')
        need(hasher.hexdigest() == expected_pin and self._describe(port) == stamp, 'image-pack-hash-stamp')
        evidence = Evidence(self.plan.manifest_pin, self.plan.context_pin, sha(self.plan.index), expected_pin, stamp.bytes)
        return View(self, port, stamp, offsets, evidence, _VIEW_TOKEN)


class View:
    """Opaque bytes and original metadata only; never a semantic Reader/export."""
    __slots__ = ('_operation', '_port', '_stamp', '_offsets', '_evidence')

    def __init__(self, operation, port, stamp, offsets, evidence, token):
        need(token is _VIEW_TOKEN, 'image-view-construction')
        self._operation, self._port, self._stamp = operation, port, stamp
        self._offsets, self._evidence = dict(offsets), evidence

    @property
    def evidence(self):
        return self._evidence

    def entries(self):
        return self._operation._guard(self._entries)

    def _entries(self):
        self._operation.session.check()
        need(self._operation._describe(self._port) == self._stamp, 'image-view-changed')
        return self._operation.plan.entries

    def read(self, name, expected_sha256, offset, length):
        def action():
            logical(name)
            pin(expected_sha256)
            entry = next((e for e in self._entries() if e.path == name), None)
            need(entry is not None and entry.kind == 'file' and entry.sha256 == expected_sha256, 'image-member-reference')
            integer(offset, 0, entry.bytes)
            integer(length, 0, RANGE)
            need(offset+length <= entry.bytes, 'image-member-range')
            # Rehash the complete member before returning a bounded slice, even
            # for an empty range. Never trust a successful earlier range alone.
            op, hasher, chunks, at = self._operation, hashlib.sha256(), [], 0
            op.session.reserve(rescans=1)
            while at < entry.bytes:
                size = min(RANGE, entry.bytes-at)
                raw = op._read(self._port, self._stamp, self._offsets[name]+at, size)
                hasher.update(raw)
                start, end = max(offset-at, 0), min(offset+length-at, size)
                if start < end:
                    chunks.append(raw[start:end])
                at += size
            need(hasher.hexdigest() == expected_sha256, 'image-member-hash')
            need(op._describe(self._port) == self._stamp, 'image-view-changed')
            return b''.join(chunks)
        return self._operation._guard(action)
