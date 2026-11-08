"""Positive CPython3.11.2/glibc2.36 Linux allocation profile.

The source-backed profile bounds application materialization/native allocation,
not physical I/O or hidden kernel work. M21 supplies actual admitted immutable
image/proc/resource/deployment evidence and bounded per-instance source reads.
"""
import hashlib
import os
import stat
import sys
from dataclasses import asdict, dataclass

from .codec import digest, integer, need, pin, Refused
from .guardian import GuardianAuthority, LiveGuardian, token_from
from .native import CursorProof, bounded_file, observe_host

SUPPORTED_FILESYSTEMS = frozenset(('ext4', 'overlay', 'tmpfs'))
BUILD_FILE_BYTES = 16777216


@dataclass
class SourceCredits:
    """Shared entered-instance SOURCE balance, never a payload/raw allowance.

    M21 passes its actual source-instance counter/adapter here, including other
    source/driver reads. A standalone counter is for a new isolated instance.
    """
    used: int = 0
    denied: bool = False

    def __post_init__(self):
        integer(self.used, 0, 1207959552)

    def reserve(self, count):
        integer(count, 0, 1207959552)
        need(not self.denied, 'source-instance-denied')
        if self.used+count > 1207959552:
            self.denied = True
            raise Refused('source-instance-byte-bound')
        self.used += count


def mount_profile(directory_fd):
    info = bounded_file('/proc/self/fdinfo/'+str(directory_fd), 4096)
    ids = [line.split(b':', 1)[1].strip() for line in info.splitlines() if line.startswith(b'mnt_id:')]
    need(len(ids) == 1 and ids[0].isdigit(), 'profile-mount-id')
    mounts = bounded_file('/proc/self/mountinfo', 131072)
    selected = []
    for line in mounts.splitlines():
        prefix, separator, suffix = line.partition(b' - ')
        if prefix.split(b' ', 1)[0] == ids[0]:
            need(bool(separator), 'profile-mount-framing')
            namespace = os.stat('/proc/self/ns/mnt')
            selected.append(dict(filesystem=suffix.split(b' ', 1)[0].decode('ascii'),
                                 fingerprint=digest(dict(line=hashlib.sha256(line).hexdigest(),
                                                         namespace=[namespace.st_dev, namespace.st_ino]))))
    need(len(selected) == 1 and selected[0]['filesystem'] in SUPPORTED_FILESYSTEMS, 'profile-filesystem')
    return selected[0]


def filesystem(directory_fd):
    return mount_profile(directory_fd)['filesystem']


@dataclass(frozen=True)
class ProfileAdmission:
    """Expected pins from TRUSTED qualified-image/source/mount admission.

    Measuring current files is not a way to issue this evidence. M21 supplies
    authenticated deployment admission referencing independently qualified
    binary/source correspondence and actual mount/backing-name evidence.
    """
    image_id: str
    source_evidence: str
    machine: str
    python_sha256: str
    libc_sha256: str
    mount_profiles: tuple
    mount_evidence: str

    def validate(self):
        need(type(self.image_id) is str and self.image_id.startswith('sha256:'), 'profile-image-id')
        pin(self.image_id[7:])
        for value in (self.source_evidence, self.python_sha256, self.libc_sha256, self.mount_evidence):
            pin(value)
        need(self.machine in ('x86_64', 'aarch64'), 'profile-machine')
        need(type(self.mount_profiles) is tuple and 1 <= len(self.mount_profiles) <= 8, 'profile-mount-admission-bound')
        for value in self.mount_profiles:
            pin(value)

    def bind_build(self, build):
        self.validate()
        need(build['facts']['machine'] == self.machine and
             build['python_object']['sha256'] == self.python_sha256 and
             build['libc_object']['sha256'] == self.libc_sha256, 'profile-admitted-build')
        # Canonical codec uses lists; tuple is the immutable in-process API.
        admission = asdict(self)
        admission['mount_profiles'] = list(self.mount_profiles)
        return digest(dict(build=build, admission=admission))


def file_identity(path, source_reads):
    """One bounded source observation; no payload/audit credit is borrowed."""
    with open(path, 'rb', buffering=0) as stream:
        before = os.fstat(stream.fileno())
        need(stat.S_ISREG(before.st_mode) and 0 < before.st_size <= BUILD_FILE_BYTES, 'profile-build-file-bound')
        hasher = hashlib.sha256()
        remaining = before.st_size
        while remaining:
            amount = min(65536, remaining)
            source_reads.reserve(amount)
            raw = stream.read(amount)
            need(bool(raw), 'profile-build-short-read')
            remaining -= len(raw)
            hasher.update(raw)
        after = os.fstat(stream.fileno())
        need((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
             (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns), 'profile-build-changed')
    return dict(sha256=hasher.hexdigest(), bytes=before.st_size)


def observe_build(source_reads):
    """Bind actual runtime objects, not merely version strings, in CI evidence.

    At most two16MiB files and bounded proc metadata, fitting one original
    128MiB SOURCE observation. The consuming run-instance owns that source pool.
    """
    facts = observe_host()
    need(facts['machine'] in ('x86_64', 'aarch64') and facts['python'].startswith('3.11.2 ') and
         facts['libc'] == 'glibc 2.36' and sys.getfilesystemencoding() == 'utf-8', 'unsupported-allocation-profile')
    source_reads.reserve(131073)  # Includes the bounded overflow probe.
    mappings = bounded_file('/proc/self/maps', 131072)
    paths = set()
    for line in mappings.splitlines():
        columns = line.split(None, 5)
        if len(columns) == 6 and columns[5].endswith(b'/libc.so.6'):
            paths.add(columns[5].decode('utf-8'))
    need(len(paths) == 1, 'profile-libc-mapping')
    return dict(facts=facts, python_object=file_identity('/proc/self/exe', source_reads),
                libc_object=file_identity(paths.pop(), source_reads), api='cpython3.11.2-glibc2.36-linux-allocation-v1')


class LinuxHostProfile(GuardianAuthority):
    """Usable same-domain profile with positive supported cursor admission.

    Sealed guardian identity and live capabilities are observed, not asserted.
    The recorded controller policy explicitly assumes trusted procfs, an
    immutable admitted image and no snapshot/rewind/migration. Those deployment
    properties are not inferred from samples; M21 must supply them in practice.
    """
    def __init__(self, raw, *, operation, source, request, policy_evidence, admission,
                 source_reads=None):
        super().__init__(raw, operation=operation, source=source, request=request, policy_evidence=policy_evidence)
        need(type(admission) is ProfileAdmission, 'profile-admission-required')
        self.admission = admission
        self.source_reads = source_reads if source_reads is not None else SourceCredits()
        self.build = observe_build(self.source_reads)
        need(token_from(raw)['allocation_build'] == admission.bind_build(self.build), 'profile-original-build')
        # Observe the live guardian before handing out any native authority.
        super().verify(self.build['facts'])

    @classmethod
    def start_guardian(cls, operation, source, request, policy_evidence, *, admission,
                       source_reads=None):
        need(type(admission) is ProfileAdmission, 'profile-admission-required')
        source_reads = source_reads if source_reads is not None else SourceCredits()
        build = observe_build(source_reads)
        guardian = LiveGuardian.create(operation, source, request, policy_evidence,
                                       allocation_build=admission.bind_build(build))
        try:
            profile = cls(guardian.raw, operation=operation, source=source, request=request,
                          policy_evidence=policy_evidence, admission=admission, source_reads=source_reads)
            # The expected admission is retained through every reconstruction.
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
        mount = mount_profile(directory_fd)
        need(mount['fingerprint'] in self.admission.mount_profiles, 'profile-unadmitted-mount')
        evidence = digest(dict(build=self.build, mount=mount, name_max=name_max,
                               guardian=domain.evidence, source_evidence=self.admission.source_evidence,
                               mount_evidence=self.admission.mount_evidence))
        # glibc2.36 opendir.c clamps native buffer to1MiB. Extra64KiB
        # conservatively covers DIR/allocator/iterator overhead. Entry8KiB
        # covers a255-byte name, worst-case Unicode, encoding, DirEntry/stat
        # temporaries and the retained typed Entry. No per-next I/O claim.
        return CursorProof(domain.profile, evidence, 255, 8192, 1114112, 24, 24)
