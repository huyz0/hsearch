#!/usr/bin/env python3
"""Qualify an immutable source bundle inside the existing capped owned workload."""
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import stat
import subprocess
import sys


def need(condition, message):
    if not condition:
        raise RuntimeError(message)


def main():
    config = Path("/coverage-source.json")
    need(config.is_file() and config.stat().st_size <= 4096, "Missing bounded coverage source snapshot")
    pin = json.loads(config.read_text())
    need(pin["schema_version"] == 2 and type(pin["head"]) is str and re.fullmatch(r"[0-9a-f]{40}", pin["head"]) and
         type(pin["base"]) is str and (pin["base"] == "parentless-root" or re.fullmatch(r"[0-9a-f]{40}", pin["base"])) and
         re.fullmatch(r"[0-9a-f]{64}", pin["bundle_sha256"]) and re.fullmatch(r"[0-9a-f]{64}", pin["source_fingerprint"]), "Invalid frozen coverage identity")
    bundle = Path("/coverage-source.bundle")
    need(stat.S_ISREG(bundle.stat().st_mode) and 0 < bundle.stat().st_size <= 128 * 1024**2, "Invalid source bundle")
    digest = hashlib.sha256()
    with bundle.open("rb") as source:
        while chunk := source.read(65536):
            digest.update(chunk)
    need(digest.hexdigest() == pin["bundle_sha256"], "Frozen source bundle hash mismatch")
    checkout = Path("/target/coverage-qualified-source")
    need(not checkout.exists(), "Preserve existing qualified source; do not reuse it")
    subprocess.run(["git", "clone", "--no-checkout", str(bundle), str(checkout)], check=True, timeout=60)
    subprocess.run(["git", "-C", str(checkout), "checkout", "--detach", pin["head"]], check=True, timeout=30)
    task = runpy.run_path(str(checkout / "tools/task.py"))
    before = task["source_fingerprint"](checkout, pin["head"], [])
    need(before["clean"] and before["head"] == pin["head"] and before["digest"] == pin["source_fingerprint"], "Linux source copy differs from frozen host source")
    env = dict(os.environ, CARGO_TARGET_DIR="/target/coverage-corpus", PYTHONDONTWRITEBYTECODE="1")
    subprocess.run([sys.executable, str(checkout / "tools/coverage.py"), "run", "--base", pin["base"],
                    "--output", "/coverage-evidence/collection"], cwd=checkout, env=env, check=True)
    need(task["source_fingerprint"](checkout, pin["head"], []) == before, "Coverage source changed during qualification")
    print(json.dumps({"local_native_coverage": "pass", "head": pin["head"], "host_copy_source_binding": "pass"}), flush=True)


if __name__ == "__main__":
    main()
