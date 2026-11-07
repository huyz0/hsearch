"""Pinned incoming byte primitives only; no semantic authority or evaluator imports."""
import hashlib, json, os, re, stat
from pathlib import Path

class Refused(ValueError):
    """Fixed, publicly safe failure code only."""


def need(ok, code):
    if not ok:
        raise Refused(code)


def keys(value, names):
    need(isinstance(value, dict) and set(value) == set(names.split()), 'schema')


def sha(value):
    need(isinstance(value, str) and re.fullmatch('[a-f0-9]{64}', value), 'digest')
    return value


def sid(value):
    need(isinstance(value, str) and re.fullmatch('[A-Za-z0-9][A-Za-z0-9_-]{0,63}', value), 'identity')


def integer(value, lower, upper):
    need(type(value) is int and lower <= value <= upper, 'bound')


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=True,
                      allow_nan=False).encode('utf-8')


def digest(value):
    return hashlib.sha256(encoded(value)).hexdigest()


def regular(path, limit, expected=None, *, retain=True):
    """Hash with bounded streaming and identity bookends; no diagnostic payload."""
    path = Path(path).absolute()
    need(path.resolve() == path and not path.is_symlink(), 'path')
    fd = None
    try:
        fd = os.open(path, os.O_RDONLY | getattr(os, 'O_BINARY', 0) |
                     getattr(os, 'O_NOFOLLOW', 0) | getattr(os, 'O_NONBLOCK', 0))
        a = os.fstat(fd)
        need(stat.S_ISREG(a.st_mode) and a.st_nlink == 1 and a.st_size <= limit, 'file')
        hasher = hashlib.sha256(); parts = []; length = 0
        with os.fdopen(fd, 'rb', closefd=False) as stream:
            while True:
                chunk = stream.read(min(65536, limit - length + 1))
                if not chunk:
                    break
                length += len(chunk); need(length <= limit, 'file-size'); hasher.update(chunk)
                if retain:
                    parts.append(chunk)
        b, c = os.fstat(fd), path.stat()
        identity = lambda x: (x.st_dev, x.st_ino, x.st_size, x.st_mtime_ns, x.st_nlink)
        need(identity(a) == identity(b) == identity(c) and length == a.st_size, 'file-changed')
        actual = hasher.hexdigest()
        need(expected is None or actual == sha(expected), 'file-digest')
        return b''.join(parts) if retain else {'sha256': actual, 'bytes': length}
    except OSError:
        raise Refused('file-io') from None
    finally:
        if fd is not None:
            os.close(fd)
