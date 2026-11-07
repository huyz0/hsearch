"""Live, same-owner Linux time-domain handoff under a declared deployment policy.

The controller supplies policy evidence for controlled procfs and no snapshot,
rewind or migration. This module cannot detect invisible whole-VM rollback.
It holds kernel handles; it grants no artifact, lease or human authority.
"""
from dataclasses import dataclass
import os
import secrets
import select

from .codec import decode, digest, encode, fields, integer, need, pin, Refused
from .context import Domain
from .native import bounded_file, observe_host

TOKEN_BYTES = 8192
TOKEN_FIELDS = ('version pid birth uid namespace_fd namespace_device namespace_inode '
                'token_fd token_device token_inode boot namespace software policy '
                'operation source request nonce profile allocation_build')


def birth(pid):
    raw = bounded_file('/proc/'+str(pid)+'/stat', 4096)
    _, separator, rest = raw.rpartition(b') ')
    need(bool(separator), 'guardian-proc-stat')
    columns = rest.split()
    need(len(columns) > 19 and columns[19].isdigit(), 'guardian-birth')
    return integer(int(columns[19]), 1)


def software(facts):
    return digest({key: facts[key] for key in ('python', 'implementation', 'machine', 'kernel', 'libc')})


def token_from(raw):
    need(type(raw) is bytes and len(raw) <= TOKEN_BYTES, 'guardian-token-bound')
    value = decode(raw)
    fields(value, TOKEN_FIELDS)
    need(type(value['version']) is int and value['version'] == 1, 'guardian-version')
    for name in ('pid', 'birth', 'namespace_fd', 'namespace_device', 'namespace_inode',
                 'token_fd', 'token_device', 'token_inode', 'uid'):
        integer(value[name])
    need(value['pid'] > 0 and value['birth'] > 0, 'guardian-process')
    for name in ('boot', 'namespace', 'software', 'policy', 'operation', 'source', 'request', 'nonce', 'profile'):
        pin(value[name])
    if value['allocation_build'] is not None:
        pin(value['allocation_build'])
    expected = digest(dict(api='linux-live-guardian-v1', software=value['software'], policy=value['policy'],
                           allocation_build=value['allocation_build']))
    need(value['profile'] == expected, 'guardian-profile')
    return value


def seals():
    import fcntl
    return fcntl.F_SEAL_GROW | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_WRITE | fcntl.F_SEAL_SEAL


@dataclass
class LiveGuardian:
    """Keep this object in the original live controller, not a recovery worker.

    Its immutable sealed memfd binds exact source/profile/operation/nonce and
    held namespace FD. Recovery opens a pidfd and proves the same live process,
    birth, owner, namespace and sealed token at both bookends. Never respawn it
    under an existing proof. M21 owns process supervision and deployment policy.
    """
    raw: bytes
    namespace_fd: int
    token_fd: int
    closed: bool = False

    @classmethod
    def create(cls, operation, source, request, policy_evidence, *, allocation_build=None):
        facts = observe_host()
        import fcntl
        for value in (operation, source, request, policy_evidence):
            pin(value)
        if allocation_build is not None:
            pin(allocation_build)
        need(hasattr(os, 'pidfd_open') and hasattr(os, 'memfd_create'), 'guardian-native-capability')
        namespace_fd = token_fd = None
        try:
            namespace_fd = os.open('/proc/self/ns/time', os.O_RDONLY | os.O_CLOEXEC)
            namespace = os.fstat(namespace_fd)
            need(digest([namespace.st_dev, namespace.st_ino]) == facts['namespace'], 'guardian-namespace')
            token_fd = os.memfd_create('retention-context-guardian-v1', os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
            token = os.fstat(token_fd)
            value = dict(version=1, pid=os.getpid(), birth=birth(os.getpid()), uid=os.getuid(),
                         namespace_fd=namespace_fd, namespace_device=namespace.st_dev,
                         namespace_inode=namespace.st_ino, token_fd=token_fd,
                         token_device=token.st_dev, token_inode=token.st_ino,
                         boot=facts['boot'], namespace=facts['namespace'], software=software(facts),
                         policy=policy_evidence, operation=operation, source=source, request=request,
                         nonce=secrets.token_hex(32), allocation_build=allocation_build)
            value['profile'] = digest(dict(api='linux-live-guardian-v1', software=value['software'],
                                          policy=policy_evidence, allocation_build=allocation_build))
            raw = encode(value)
            token_from(raw)
            need(os.write(token_fd, raw) == len(raw), 'guardian-token-write')
            fcntl.fcntl(token_fd, fcntl.F_ADD_SEALS, seals())
            guardian = cls(raw, namespace_fd, token_fd)
            verify_guardian(raw, facts)
            return guardian
        except Exception:
            for fd in (token_fd, namespace_fd):
                if fd is not None:
                    os.close(fd)
            raise

    def close(self):
        if self.closed:
            return
        self.closed = True
        errors = []
        for fd in (self.namespace_fd, self.token_fd):
            try:
                os.close(fd)
            except OSError as exc:
                errors.append(exc)
        if errors:
            raise Refused('guardian-close-uncertain') from errors[0]

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def verify_guardian(raw, facts):
    """Kernel-backed local identity. Numeric namespace IDs alone never suffice."""
    need(hasattr(os, 'pidfd_open') and hasattr(os, 'memfd_create'), 'guardian-native-capability')
    import fcntl
    value = token_from(raw)
    need(value['uid'] == os.getuid() and value['boot'] == facts['boot'] and
         value['namespace'] == facts['namespace'] and value['software'] == software(facts),
         'guardian-domain-or-software')
    pidfd = namespace_fd = token_fd = None
    try:
        pidfd = os.pidfd_open(value['pid'], 0)
        poll = select.poll()
        poll.register(pidfd, select.POLLIN | select.POLLHUP | select.POLLERR)
        need(not poll.poll(0), 'guardian-dead')
        root = '/proc/'+str(value['pid'])
        need(os.stat(root).st_uid == value['uid'] and birth(value['pid']) == value['birth'], 'guardian-birth-owner')
        namespace_fd = os.open(root+'/fd/'+str(value['namespace_fd']), os.O_RDONLY | os.O_CLOEXEC)
        namespace = os.fstat(namespace_fd)
        need((namespace.st_dev, namespace.st_ino) == (value['namespace_device'], value['namespace_inode']) and
             digest([namespace.st_dev, namespace.st_ino]) == facts['namespace'], 'guardian-held-namespace')
        token_fd = os.open(root+'/fd/'+str(value['token_fd']), os.O_RDONLY | os.O_CLOEXEC)
        token = os.fstat(token_fd)
        need((token.st_dev, token.st_ino) == (value['token_device'], value['token_inode']), 'guardian-held-token')
        need(fcntl.fcntl(token_fd, fcntl.F_GET_SEALS) & seals() == seals(), 'guardian-token-unsealed')
        need(os.pread(token_fd, TOKEN_BYTES+1, 0) == raw, 'guardian-token-binding')
        need(not poll.poll(0) and birth(value['pid']) == value['birth'], 'guardian-lifetime-bookend')
        return Domain(value['boot'], value['namespace'], value['profile'], digest(value))
    except OSError as exc:
        raise Refused('guardian-unavailable') from exc
    finally:
        for fd in (token_fd, namespace_fd, pidfd):
            if fd is not None:
                os.close(fd)


class GuardianAuthority:
    """Positive live Linux clock profile under the recorded deployment assumption."""
    def __init__(self, raw, *, operation, source, request, policy_evidence):
        value = token_from(raw)
        need((value['operation'], value['source'], value['request'], value['policy']) ==
             (pin(operation), pin(source), pin(request), pin(policy_evidence)), 'guardian-admission-binding')
        self.raw = raw

    def bind(self, operation, source, request):
        value = token_from(self.raw)
        need((value['operation'], value['source'], value['request']) ==
             (operation, source, request), 'guardian-context-binding')

    def verify(self, facts):
        return verify_guardian(self.raw, facts)
