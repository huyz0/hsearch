#!/usr/bin/env python3
"""Generate isolated, short-lived development credentials. Never production credentials."""
import hashlib
import json
import os
from pathlib import Path
import secrets
import subprocess

BASE = Path("/hsearch-state")
FILES = ("ca/ca.key", "ca/ca.crt", "server/rustfs_key.pem", "server/rustfs_cert.pem",
         "server/ca.crt", "client/ca.crt", "client/wrong-ca.crt", "credentials/access", "credentials/secret")


def openssl(*args):
    subprocess.run(["openssl", *args], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)


def main():
    os.umask(0o077)
    marker = BASE / "ca/initialized.json"
    if marker.exists():
        expected = json.loads(marker.read_text())
        actual = {p: hashlib.sha256((BASE / p).read_bytes()).hexdigest() for p in FILES}
        if actual != expected:
            raise RuntimeError("Existing development credentials changed; use a fresh namespace")
        openssl("x509", "-in", str(BASE / "server/rustfs_cert.pem"), "-checkend", "3600", "-noout")
        print("Development CA and credentials already initialized")
        return
    if any((BASE / p).exists() for p in FILES):
        raise RuntimeError("Partial credential initialization; preserve it and use a fresh namespace")
    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=hsearch-development-ca",
            "-keyout", str(BASE / "ca/ca.key"), "-out", str(BASE / "ca/ca.crt"),
            "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    openssl("req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=rustfs", "-keyout", str(BASE / "server/rustfs_key.pem"),
            "-out", "/tmp/server.csr")
    Path("/tmp/server.ext").write_text("basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:rustfs,DNS:localhost,IP:127.0.0.1\n")
    openssl("x509", "-req", "-in", "/tmp/server.csr", "-CA", str(BASE / "ca/ca.crt"), "-CAkey", str(BASE / "ca/ca.key"),
            "-CAcreateserial", "-days", "2", "-extfile", "/tmp/server.ext", "-out", str(BASE / "server/rustfs_cert.pem"))
    for name in ("server/ca.crt", "client/ca.crt"):
        (BASE / name).write_bytes((BASE / "ca/ca.crt").read_bytes())
    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=untrusted-development-ca",
            "-keyout", "/tmp/wrong.key", "-out", str(BASE / "client/wrong-ca.crt"))
    (BASE / "credentials/access").write_text("hs" + secrets.token_hex(12))
    (BASE / "credentials/secret").write_text(secrets.token_hex(32))
    hashes = {p: hashlib.sha256((BASE / p).read_bytes()).hexdigest() for p in FILES}
    marker.write_text(json.dumps(hashes, sort_keys=True) + "\n")
    print("Initialized short-lived development CA and unique storage credentials")


if __name__ == "__main__":
    main()
