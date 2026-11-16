#!/usr/bin/env python3
"""Independent encoder for the M05.03 encrypted block format, written from docs/plan/security.md and
docs/plan/formats.md alone. It shares no code with hsearch-security. `python3 block_v1.py` writes
block_v1.hex; `python3 block_v1.py --check` fails if the file differs."""
import struct
import sys
from pathlib import Path

from cryptography.hazmat.primitives.ciphers.aead import AESGCM

DEK = bytes([0x42] * 32)
OBJECT = bytes([0x11] * 16)
DOMAIN = bytes([0x22] * 16)
KEY_REF = b"test/acct/kat"
VERSION = b"v1"
WRAPPED = bytes(range(48))
BLOCK = 32
PLAINTEXT = bytes(range(100))


def nonce(counter: int) -> bytes:
    return b"\0\0\0\0" + struct.pack(">Q", counter)


def seal() -> bytes:
    prefix = (
        b"HSEB" + bytes([1, 1]) + struct.pack("<H", 0) + OBJECT + DOMAIN
        + struct.pack("<I", BLOCK) + struct.pack("<Q", len(PLAINTEXT))
    )
    assert len(prefix) == 52
    aes = AESGCM(DEK)
    out = prefix + struct.pack("<HHH", len(KEY_REF), len(VERSION), len(WRAPPED))
    out += KEY_REF + VERSION + WRAPPED
    out += aes.encrypt(nonce(0), b"", b"hs-block-header\0\x01" + prefix)
    for i in range(0, (len(PLAINTEXT) + BLOCK - 1) // BLOCK):
        chunk = PLAINTEXT[i * BLOCK:(i + 1) * BLOCK]
        aad = b"hs-block\0\x01" + prefix + struct.pack("<I", i)
        out += aes.encrypt(nonce(i + 1), chunk, aad)
    return out


if __name__ == "__main__":
    path = Path(__file__).with_name("block_v1.hex")
    text = seal().hex() + "\n"
    if "--check" in sys.argv:
        sys.exit(0 if path.read_text() == text else 1)
    path.write_text(text)
