"""Linux FD cursor and host clock ports, conditional on independent host proof.

No bundled host attestation exists. These safe APIs cannot establish a bound on
hidden native work or arbitrary blocked syscall latency. Unknown proof refuses.
"""
from dataclasses import dataclass
import hashlib
import os
import platform
import stat
import sys
import time
from typing import Protocol

from .codec import digest, integer, need, pin, Refused
from .context import Domain, Sample


def bounded_file(path, cap):
    with open(path, 'rb', buffering=0) as stream:
        raw = stream.read(cap+1)
    need(len(raw) <= cap, 'host-evidence-bound')
    return raw


def observe_host():
    """Observations are not trusted initial-domain or no-rewind attestations."""
    need(sys.platform == 'linux' and sys.implementation.name == 'cpython' and
         hasattr(time, 'CLOCK_BOOTTIME'), 'unsupported-native-host')
    boot = bounded_file('/proc/sys/kernel/random/boot_id', 64).strip()
    need(len(boot) == 36 and all(c in b'0123456789abcdef-' for c in boot), 'boot-shape')
    self_ns = os.stat('/proc/self/ns/time')
    try:
        init_ns = os.stat('/proc/1/ns/time')
        pid1_namespace = digest([init_ns.st_dev, init_ns.st_ino])
    except PermissionError:
        # PID1 may be inaccessible. Even equality would not establish the
        # initial host namespace. Only the independent authority can do that.
        pid1_namespace = None
    libc = os.confstr('CS_GNU_LIBC_VERSION')
    need(type(libc) is str and len(libc) <= 128, 'libc-observation')
    return dict(boot=hashlib.sha256(boot).hexdigest(),
                namespace=digest([self_ns.st_dev, self_ns.st_ino]),
                pid1_namespace=pid1_namespace,
                python=sys.version, implementation=sys.implementation.name,
                machine=platform.machine(), kernel=platform.release(), libc=libc)


class DomainAuthority(Protocol):
    def verify(self, facts: dict) -> Domain:
        """Independently attest proc provenance, initial time-domain and no rewind."""
        ...


class LinuxClock:
    def __init__(self, authority: DomainAuthority):
        self.authority = authority

    def sample(self):
        before = observe_host()
        domain = self.authority.verify(before)
        need(type(domain) is Domain, 'domain-attestation')
        domain.validate()
        need(domain.boot == before['boot'] and domain.namespace == before['namespace'], 'domain-evidence-match')
        after = observe_host()
        need(before == after and self.authority.verify(after) == domain, 'host-bookends')
        mono = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
        boot = time.clock_gettime_ns(time.CLOCK_BOOTTIME)
        wall = time.time_ns()
        return Sample(mono, boot, wall, domain)


@dataclass(frozen=True)
class CursorProof:
    profile: str
    evidence: str
    name_bytes: int
    entry_allocation_bytes: int
    stream_allocation_bytes: int
    native_work_bytes: int
    max_streams: int
    max_depth: int

    def validate(self):
        pin(self.profile)
        pin(self.evidence)
        integer(self.name_bytes, 1, 4096)
        integer(self.entry_allocation_bytes, 8*self.name_bytes+4096, 1048576)
        # A conservative qualified glibc2.36 candidate includes its 1MiB
        # maximum buffer PLUS DIR/Python/allocator overhead. Version text alone
        # never supplies that qualification or the separate native-work bound.
        integer(self.stream_allocation_bytes, 1048576+65536, 4194304)
        integer(self.native_work_bytes, 1, 268435456)
        integer(self.max_streams, 1, 24)
        integer(self.max_depth, 1, 24)


class CursorAuthority(Protocol):
    def verify(self, facts: dict, directory: os.stat_result, name_max: int) -> CursorProof:
        """Attest exact implementation/filesystem bounds, including hidden work."""
        ...


@dataclass(frozen=True)
class Entry:
    name: str
    device: int
    inode: int
    mode: int
    size: int


class CursorPool:
    """One operation-wide stack/FD/allocation pool backed by its audit credits.

    Closing releases live capacity, never cumulative work/byte/entry credits.
    The caller supplies the original held directory FD; each cursor owns a
    newly opened FD and scandir's duplicate. Its borrowed input FD is also
    conservatively reserved until close, even if the caller closes it earlier.
    """
    def __init__(self, session, authority: CursorAuthority, memory_bytes, fd_limit):
        integer(memory_bytes, 1, 4294967296)
        integer(fd_limit, 3, 256)
        self.session, self.authority = session, authority
        self.memory_bytes, self.fd_limit = memory_bytes, fd_limit
        self.active = []
        self.memory_used = self.fds_used = 0
        self.denied = False

    def open(self, directory_fd, *, cap=4096):
        self.session.reserve(rescans=1)
        return self._open(directory_fd, depth=1, cap=cap)

    def _open(self, directory_fd, *, depth, cap):
        need(not self.denied, 'cursor-pool-denied')
        integer(directory_fd)
        integer(depth, 1, 24)
        integer(cap, 0, 4096)
        self.session.check()
        # The held FD's identity/name/profile inspection is itself charged.
        self.session.reserve(audit_bytes=4096)
        info = os.fstat(directory_fd)
        need(stat.S_ISDIR(info.st_mode), 'cursor-not-directory')
        facts = observe_host()
        name_max = os.fpathconf(directory_fd, 'PC_NAME_MAX')
        proof = self.authority.verify(facts, info, name_max)
        need(type(proof) is CursorProof, 'cursor-attestation')
        proof.validate()
        need(proof.profile == self.session._read()[2].start.domain.profile, 'cursor-profile')
        need(0 < name_max <= proof.name_bytes, 'filesystem-name-bound')
        charge = proof.stream_allocation_bytes+proof.entry_allocation_bytes
        need(len(self.active) < proof.max_streams and depth <= proof.max_depth and
             self.memory_used+charge <= self.memory_bytes and self.fds_used+3 <= self.fd_limit,
             'cursor-capacity')
        self.session.reserve(audit_bytes=charge)
        self.memory_used += charge
        self.fds_used += 3
        fd = iterator = None
        try:
            # dup() would share the original directory offset. openat('.')
            # gives this owned stream its own open-file description instead.
            fd = os.open('.', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                         dir_fd=directory_fd)
            self.session.check()
            fresh = os.fstat(fd)
            need((fresh.st_dev, fresh.st_ino) == (info.st_dev, info.st_ino), 'cursor-directory-changed')
            iterator = os.scandir(fd)
            self.session.check()
            cursor = Cursor(self, fd, iterator, proof, depth, cap, charge)
            self.active.append(cursor)
            return cursor
        except Exception:
            errors = []
            if iterator is not None:
                try:
                    iterator.close()
                except Exception as exc:
                    errors.append(exc)
            if fd is not None:
                try:
                    os.close(fd)
                except Exception as exc:
                    errors.append(exc)
            if not errors:
                self.memory_used -= charge
                self.fds_used -= 3
            self.denied = True
            try:
                self.session.deny()
            except Exception:
                pass
            raise

    def close(self):
        errors = []
        for cursor in tuple(reversed(self.active)):
            try:
                cursor.close()
            except Exception as exc:
                errors.append(exc)
        if errors:
            raise Refused('cursor-close-uncertain') from errors[0]


class Cursor:
    def __init__(self, pool, fd, iterator, proof, depth, cap, allocation):
        self.pool, self.fd, self.iterator, self.proof = pool, fd, iterator, proof
        self.depth, self.cap, self.allocation = depth, cap, allocation
        self.delivered = self.probes = 0
        self.closed = self.eof = False
        self.close_error = None

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def next(self):
        need(not self.closed and not self.eof and not self.pool.denied, 'cursor-closed')
        session = self.pool.session
        try:
            # Reserve worst-case allocation and hidden native read work even
            # for the one EOF/overflow probe. No eager wrapper or free EOF.
            session.reserve(audit_bytes=self.proof.entry_allocation_bytes+self.proof.native_work_bytes)
            self.probes += 1
            try:
                native = next(self.iterator)
            except StopIteration:
                session.check()
                self.eof = True
                self.close()
                session.check()
                return None
            session.check()
            need(self.delivered < self.cap, 'cursor-overflow')
            name = native.name
            need(type(name) is str and 0 < len(name) <= self.proof.name_bytes and
                 '/' not in name and '\x00' not in name and name not in {'.', '..'}, 'cursor-name')
            # Transient encoding is covered by the worst-case entry allowance.
            need(len(os.fsencode(name)) <= self.proof.name_bytes, 'cursor-name-bytes')
            session.reserve(entries=1)
            session.check()
            info = os.stat(name, dir_fd=self.fd, follow_symlinks=False)
            session.check()
            need(not stat.S_ISLNK(info.st_mode), 'cursor-alias')
            result = Entry(name, info.st_dev, info.st_ino, info.st_mode, info.st_size)
            self.delivered += 1
            return result
        except Exception:
            self.pool.denied = True
            try:
                session.deny()
            except Exception:
                pass
            self.close()
            raise

    def open_child(self, entry, *, cap=4096):
        need(type(entry) is Entry and not self.closed and not self.pool.denied, 'child-state')
        need(type(entry.name) is str and 0 < len(entry.name) <= self.proof.name_bytes and
             '/' not in entry.name and '\x00' not in entry.name and entry.name not in {'.', '..'}, 'child-name')
        need(stat.S_ISDIR(entry.mode), 'child-not-directory')
        self.pool.session.reserve(audit_bytes=4096)
        fd = None
        try:
            # Reserve the temporary descriptor before open, beyond both cursor
            # descriptors already held by the pool.
            need(self.pool.fds_used+1 <= self.pool.fd_limit, 'child-fd-capacity')
            fd = os.open(entry.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=self.fd)
            self.pool.fds_used += 1
            self.pool.session.check()
            info = os.fstat(fd)
            need((info.st_dev, info.st_ino) == (entry.device, entry.inode), 'child-identity')
            child = self.pool._open(fd, depth=self.depth+1, cap=cap)
        except Exception:
            self.pool.denied = True
            try:
                self.pool.session.deny()
            except Exception:
                pass
            raise
        finally:
            if fd is not None:
                os.close(fd)
                self.pool.fds_used -= 1
        try:
            self.pool.session.check()
        except Exception:
            child.close()
            raise
        return child

    def close(self):
        if self.closed:
            if self.close_error is not None:
                raise Refused('cursor-close-uncertain') from self.close_error
            return
        self.closed = True
        errors = []
        try:
            self.iterator.close()
        except Exception as exc:
            errors.append(exc)
        try:
            os.close(self.fd)
        except Exception as exc:
            errors.append(exc)
        if errors:
            self.close_error = errors[0]
            self.pool.denied = True
            try:
                self.pool.session.deny()
            except Exception:
                pass
            raise Refused('cursor-close-uncertain') from errors[0]
        self.pool.active.remove(self)
        self.pool.memory_used -= self.allocation
        self.pool.fds_used -= 3
