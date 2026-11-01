#!/usr/bin/env python3
"""Build the pinned external development CLI inside the capped build workload."""
import hashlib
import json
from pathlib import Path
import re
import runpy
import shutil
import stat
import subprocess


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def main():
    profile = json.loads(Path("/opt/hsearch/navigation-tools.json").read_text())
    tool = profile["lsp_cli"]
    revision = tool["revision"]
    if tool["source"] != "https://github.com/huyz0/lsp-cli" or not re.fullmatch("[a-f0-9]{40}", revision):
        raise RuntimeError("Unknown navigation source pin")
    source = Path("/cargo/navigation-source") / revision
    source.mkdir(parents=True, exist_ok=True)
    if not (source / ".git").exists():
        subprocess.run(["git", "init", "-q", str(source)], check=True)
        subprocess.run(["git", "-C", str(source), "fetch", "--depth=1", tool["source"], revision], check=True)
        subprocess.run(["git", "-C", str(source), "checkout", "--detach", "FETCH_HEAD"], check=True)
    actual = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"]).decode().strip()
    if actual != revision or subprocess.check_output(["git", "-C", str(source), "status", "--porcelain"]).strip():
        raise RuntimeError("Navigation source identity/cleanliness changed")
    lock_hash = hashlib.sha256((source / "Cargo.lock").read_bytes()).hexdigest()
    if lock_hash != tool["cargo_lock_sha256"]:
        raise RuntimeError("Navigation lockfile pin changed")
    subprocess.run(["cargo", "fetch", "--locked", "--manifest-path", str(source / "Cargo.toml")], check=True)
    subprocess.run(["cargo", "build", "--release", "--locked", "--offline", "--jobs", "2", "--bin", "lsp",
                    "--manifest-path", str(source / "Cargo.toml"), "--target-dir", "/target/navigation-build"], check=True)
    binary = Path("/target/navigation-build/release/lsp")
    if not stat.S_ISREG(binary.lstat().st_mode) or binary.stat().st_size > 64 * 1024**2:
        raise RuntimeError("Unexpected navigation executable type/size")
    version = subprocess.check_output([str(binary), "--version"], timeout=10).decode().strip()
    if version != "lsp " + tool["version"]:
        raise RuntimeError("Navigation executable version mismatch")
    output = Path("/navigation-output")
    with binary.open("rb") as original, (output / "lsp").open("xb") as target:
        shutil.copyfileobj(original, target, 65536)
    digest = hashlib.sha256((output / "lsp").read_bytes()).hexdigest()
    result = {"revision": revision, "cargo_lock_sha256": lock_hash, "version": version, "binary_sha256": digest,
              "profile_sha256": hashlib.sha256(canonical(profile)).hexdigest(),
              "rustc": subprocess.check_output(["rustc", "--version"]).decode().strip()}
    (output / "attestation.json").write_text(json.dumps(result, sort_keys=True) + "\n")
    correctness = runpy.run_path(str(Path(__file__).with_name("install_correctness.py")))["install"](
        output, json.loads(Path("/opt/hsearch/correctness-tools.json").read_text()))
    print(json.dumps({"navigation_artifact": result, "correctness_artifact": correctness}), flush=True)


if __name__ == "__main__":
    main()
