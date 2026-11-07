"""Positive CPython3.11.2/glibc2.36 Linux allocation profile.

The source-backed profile bounds application materialization/native allocation,
not physical I/O or hidden kernel work. M21 supplies actual admitted immutable
image/proc/resource/deployment evidence and bounded per-instance source reads.
"""
import hashlib
import os
import stat
import sys

from .codec import digest, need, Refused
from .guardian import GuardianAuthority, LiveGuardian, token_from
from .native import CursorProof, bounded_file, observe_host

SUPPORTED_FILESYSTEMS = frozenset(('ext4', 'overlay', 'tmpfs'))
BUILD_FILE_BYTES = 16777216


def filesystem(directory_fd):
    info = bounded_file('/proc/self/fdinfo/'+str(directory_fd), 4096)
    ids = [line.split(b':', 1)[1].strip() for line in info.splitlines() if line.startswith(b'mnt_id:')]
    need(len(ids) == 1 and ids[0].isdigit(), 'profile-mount-id')
    mounts = bounded_file('/proc/self/mountinfo', 131072)
    selected = []
    for line in mounts.splitlines():
        prefix, separator, suffix = line.partition(b' - ')
        if prefix.split(b' ', 1)[0] == ids[0]:
            need(bool(separator), 'profile-mount-framing')
            selected.append(suffix.split(b' ', 1)[0].decode('ascii'))
    need(len(selected) == 1 and selected[0] in SUPPORTED_FILESYSTEMS, 'profile-filesystem')
    return selected[0]


def file_identity(path):
    """One bounded source observation; no payload/audit credit is borrowed."""
    with open(path, 'rb', buffering=0) as stream:
        before = os.fstat(stream.fileno())
        need(stat.S_ISREG(before.st_mode) and 0 < before.st_size <= BUILD_FILE_BYTES, 'profile-build-file-bound')
        hasher = hashlib.sha256()
        remaining = before.st_size
        while remaining:
            raw = stream.read(min(65536, remaining))
            need(bool(raw), 'profile-build-short-read')
            remaining -= len(raw)
            hasher.update(raw)
        after = os.fstat(stream.fileno())
        need((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
             (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns), 'profile-build-changed')
    return dict(sha256=hasher.hexdigest(), bytes=before.st_size)


def observe_build():
    """Bind actual runtime objects, not merely version strings, in CI evidence.

    At most two16MiB files and bounded proc metadata, fitting one original
    128MiB SOURCE observation. The consuming run-instance owns that source pool.
    """
    facts = observe_host()
    need(facts['machine'] in ('x86_64', 'aarch64') and facts['python'].startswith('3.11.2 ') and
         facts['libc'] == 'glibc 2.36' and sys.getfilesystemencoding() == 'utf-8', 'unsupported-allocation-profile')
    mappings = bounded_file('/proc/self/maps', 131072)
    paths = set()
    for line in mappings.splitlines():
        columns = line.split(None, 5)
        if len(columns) == 6 and columns[5].endswith(b'/libc.so.6'):
            paths.add(columns[5].decode('utf-8'))
    need(len(paths) == 1, 'profile-libc-mapping')
    return dict(facts=facts, python_object=file_identity('/proc/self/exe'),
                libc_object=file_identity(paths.pop()), api='cpython3.11.2-glibc2.36-linux-allocation-v1')


class LinuxHostProfile(GuardianAuthority):
    """Usable same-domain profile with positive supported cursor admission.

    Sealed guardian identity and live capabilities are observed, not asserted.
    The recorded controller policy explicitly assumes trusted procfs, an
    immutable admitted image and no snapshot/rewind/migration. Those deployment
    properties are not inferred from samples; M21 must supply them in practice.
    """
    def __init__(self, raw, *, operation, source, request, policy_evidence):
        super().__init__(raw, operation=operation, source=source, request=request, policy_evidence=policy_evidence)
        self.build = observe_build()
        need(token_from(raw)['allocation_build'] == digest(self.build), 'profile-original-build')
        # Observe the live guardian before handing out any native authority.
        super().verify(self.build['facts'])

    @classmethod
    def start_guardian(cls, operation, source, request, policy_evidence):
        build = observe_build()
        guardian = LiveGuardian.create(operation, source, request, policy_evidence,
                                       allocation_build=digest(build))
        try:
            profile = cls(guardian.raw, operation=operation, source=source, request=request,
                          policy_evidence=policy_evidence)
            return guardian, profile
        except Exception:
            guardian.close()
            raise

    def verify(self, facts, *directory):
        domain = super().verify(facts)
        need(facts == self.build['facts'], 'profile-runtime-drift')
        if not directory:
            return domain
        need(len(directory) == 3, 'profile-directory-arguments')
        info, name_max, directory_fd = directory
        need(stat.S_ISDIR(info.st_mode) and name_max == 255, 'profile-directory-name')
        mount_type = filesystem(directory_fd)
        evidence = digest(dict(build=self.build, filesystem=mount_type, name_max=name_max,
                               guardian=domain.evidence))
        # glibc2.36 opendir.c clamps native buffer to1MiB. Extra64KiB
        # conservatively covers DIR/allocator/iterator overhead. Entry8KiB
        # covers a255-byte name, worst-case Unicode, encoding, DirEntry/stat
        # temporaries and the retained typed Entry. No per-next I/O claim.
        return CursorProof(domain.profile, evidence, 255, 8192, 1114112, 24, 24)
