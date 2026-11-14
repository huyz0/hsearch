#!/usr/bin/env python3
"""Independent encoder for the hsearch on-disk formats, written from docs/plan/formats.md
alone (no Rust code shared). It writes the golden corpus; crates/format/tests/golden.rs
checks that the Rust encoder produces the same bytes and the Rust decoder reads them.

    python3 fixtures/formats/gen.py          # rewrite the corpus
    python3 fixtures/formats/gen.py --check  # fail if a file differs
"""
import hashlib
import struct
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


def crc32c(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


def seal(kind, body, kind_version=1, mandatory=0, envelope=1, flags=0, optional=0, length=None):
    head = b"HSFO" + struct.pack("<HHHHQQI", envelope, kind, kind_version, flags, mandatory, optional,
                                 len(body) if length is None else length)
    head += struct.pack("<I", crc32c(body))
    return head + struct.pack("<I", crc32c(head)) + body


def ident(n):
    return bytes([n]) * 16


def lstr(s):
    b = s.encode()
    return struct.pack("<H", len(b)) + b


def sstr(s):
    b = s.encode()
    return bytes([len(b)]) + b


def token(n):
    return bytes([n]) * 16


def fleet():
    body = struct.pack("<QHHQ", 7, 3, 1, 0b10) + token(9)
    return seal(1, body)


def lane():
    members = [ident(1), ident(2), ident(9)]
    body = struct.pack("<Q", 1) + ident(0x0D) + bytes([1]) + lstr("") + sstr("eu-west-1") + bytes([1]) + struct.pack("<H", 1)
    body += ident(0xAA) + struct.pack("<II", 1000, len(members)) + b"".join(members) + token(1)
    return seal(2, body)


def slot_header(ordinal, tok, kind):
    return ident(3) + struct.pack("<QIQ", 2, 1, ordinal) + token(tok) + bytes([kind])


def data_slot():
    inline = b'{"title":"hello"}'
    recs = ident(1) + struct.pack("<QBB", 10, 1, 1) + struct.pack("<I", len(inline)) + inline
    recs += ident(2) + struct.pack("<QBB", 11, 2, 0)
    recs += ident(1) + struct.pack("<QBB", 12, 1, 2) + ident(8) + struct.pack("<Q", 200_000_000) + bytes([5]) * 32
    return seal(3, slot_header(41, 6, 1) + struct.pack("<I", 3) + recs)


def fence_slot():
    return seal(3, slot_header(5, 2, 2) + struct.pack("<Q", 12))


def entry(name, n):
    nb = name.encode()
    return struct.pack("<H", len(nb)) + nb + ident(n) + struct.pack("<Q", n * 1000) + bytes([n]) * 32


def page():
    entries = [entry("_0.cfs", 1), entry("_0.si", 2), entry("segments_1", 3)]
    return seal(5, struct.pack("<I", len(entries)) + b"".join(entries))


def root(page_bytes):
    body = struct.pack("<Q", 3) + ident(1) + sstr("lucene-10.3") + bytes([7]) * 32 + struct.pack("<Q", 99) + ident(4)
    body += struct.pack("<I", 1) + ident(8) + struct.pack("<I", 3) + hashlib.sha256(page_bytes).digest() + token(3)
    return seal(4, body)


def mapping():
    canon = b'{"properties":{"a":{"type":"keyword"}}}'
    return seal(6, hashlib.sha256(canon).digest() + canon)


def pin():
    return seal(7, ident(1) + ident(2) + ident(3) + struct.pack("<QQ", 5, 1_700_000_000_000) + token(1))


def cursor():
    body = ident(1) + struct.pack("<Q", 4) + bytes([1]) + struct.pack("<I", 2)
    body += ident(0) + struct.pack("<Q", 0) + ident(1) + struct.pack("<Q", 10)
    return seal(8, body)


def corpus():
    pg = page()
    good = {"fleet": fleet(), "lane": lane(), "slot-data": data_slot(), "slot-fence": fence_slot(),
            "manifest-page": pg, "manifest-root": root(pg), "mapping": mapping(), "pin": pin(),
            "cursor": cursor()}
    f = fleet()
    body = f[40:]
    bad = {
        "unknown-envelope-version": seal(1, body, envelope=2),
        "unknown-kind-version": seal(1, body, kind_version=2),
        "unknown-kind": seal(99, body),
        "unknown-mandatory-capability-high": seal(1, body, mandatory=1 << 63),
        "unknown-mandatory-capability": seal(1, body, mandatory=0b1),
        "forged-length-4gib": seal(1, body, length=0xFFFFFFFF),
        "trailing-byte": f + b"\0",
        "truncated": f[:-1],
    }
    return good, bad


def main():
    good, bad = corpus()
    files = {f"good/{k}.bin": v for k, v in good.items()} | {f"bad/{k}.bin": v for k, v in bad.items()}
    check = "--check" in sys.argv
    status = 0
    for name, data in sorted(files.items()):
        path = HERE / name
        if check:
            if not path.exists() or path.read_bytes() != data:
                print("differs:", name)
                status = 1
        else:
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(data)
    print(("checked" if check else "wrote"), len(files), "files")
    return status


if __name__ == "__main__":
    sys.exit(main())
