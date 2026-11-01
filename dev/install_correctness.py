#!/usr/bin/env python3
"""Install the pinned Linux test runner into a caller-owned private tool directory."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import subprocess
import tarfile
import time
import urllib.request


def need(condition, message):
    if not condition:
        raise RuntimeError(message)


def unpack_binary(archive):
    # Bound decompression before tar metadata parsing, including PAX extensions.
    raw = bytearray()
    with gzip.GzipFile(fileobj=io.BytesIO(archive)) as compressed:
        while chunk := compressed.read(65536):
            need(len(raw) + len(chunk) <= 128 * 1024**2, "Tool archive expansion exceeds bound")
            raw.extend(chunk)
    binaries, count = [], 0
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as package:
        for member in package:
            count += 1
            path = PurePosixPath(member.name)
            need(count <= 32 and not path.is_absolute() and ".." not in path.parts and
                 (member.isfile() or member.isdir()) and 0 <= member.size <= 64 * 1024**2,
                 "Tool archive has unsafe or oversized members")
            if path.name == "cargo-nextest":
                need(member.isfile() and member.size > 0, "Tool executable must be a regular nonempty member")
                with package.extractfile(member) as source:
                    binary = source.read(64 * 1024**2 + 1)
                need(len(binary) == member.size, "Tool executable size changed")
                binaries.append(binary)
    need(len(binaries) == 1, "Tool archive must contain exactly one executable")
    return binaries[0]


def install(output, profile):
    need(platform.system() == "Linux", "Correctness tools are qualified on native Linux only")
    tool = profile["nextest"]
    arch = platform.machine()
    need(profile["schema_version"] == 1 and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", tool["version"]) and
         tool["source"] == "https://github.com/nextest-rs/nextest" and
         re.fullmatch(r"[0-9a-f]{40}", tool["revision"]) and arch in tool["architectures"], "Unknown tool profile/platform")
    pin = tool["architectures"][arch]
    expected_url = tool["source"] + "/releases/download/cargo-nextest-" + tool["version"] + "/cargo-nextest-" + tool["version"] + "-" + pin["triple"] + ".tar.gz"
    need(pin["url"] == expected_url and pin["triple"] == arch + "-unknown-linux-musl" and
         type(pin["archive_bytes"]) is int and 0 < pin["archive_bytes"] <= 16 * 1024**2 and
         re.fullmatch(r"[0-9a-f]{64}", pin["archive_sha256"]), "Unbounded or mismatched tool pin")
    output = Path(output).absolute()
    need(output.resolve() == output and output.is_dir(), "Tool output directory is absent or redirected")
    binary_path, receipt_path = output / "cargo-nextest", output / "correctness-attestation.json"
    need(not os.path.lexists(binary_path) and not os.path.lexists(receipt_path), "Preserve existing tool output")
    archive = bytearray()
    deadline = time.monotonic() + 120
    with urllib.request.urlopen(pin["url"], timeout=30) as response:
        while chunk := response.read(65536):
            need(time.monotonic() < deadline, "Tool download deadline exceeded")
            need(len(archive) + len(chunk) <= pin["archive_bytes"], "Tool download exceeds exact pin")
            archive.extend(chunk)
    need(len(archive) == pin["archive_bytes"] and hashlib.sha256(archive).hexdigest() == pin["archive_sha256"], "Tool download size/hash differs from pin")
    binary = unpack_binary(archive)
    with binary_path.open("xb") as target:
        target.write(binary)
    binary_path.chmod(0o755)
    version = subprocess.check_output([str(binary_path), "--version"], text=True, timeout=10).strip()
    need(len(version) <= 512 and version.split()[:2] == ["cargo-nextest", tool["version"]], "Installed tool version mismatch")
    receipt = {"schema_version": 1, "architecture": arch, "version": version, "revision": tool["revision"],
               "archive_sha256": pin["archive_sha256"], "binary_sha256": hashlib.sha256(binary).hexdigest(),
               "profile_sha256": hashlib.sha256(json.dumps(profile, sort_keys=True, separators=(",", ":")).encode()).hexdigest()}
    with receipt_path.open("x", encoding="utf-8", newline="\n") as target:
        target.write(json.dumps(receipt, sort_keys=True) + "\n")
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    output = args.output.absolute()
    source = Path(__file__).resolve().parents[1]
    need(output.resolve() == output and not output.is_relative_to(source), "Tool output must be outside source")
    output.mkdir(mode=0o755, parents=True, exist_ok=False)
    profile_path = Path(__file__).with_name("correctness-tools.json")
    need(profile_path.stat().st_size <= 4096, "Tool profile exceeds bound")
    profile = json.loads(profile_path.read_text(encoding="utf-8"))
    print(json.dumps({"correctness_artifact": install(output, profile)}), flush=True)


if __name__ == "__main__":
    main()
