#!/usr/bin/env python3
"""Run known development checks within the surrounding container's resource caps."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import signal
import selectors
import shutil
import subprocess
import sys
import time

COMMANDS = {"coverage": ["python3", "/harness/coverage_entry.py"], "prefetch": ["cargo", "fetch", "--locked"], "check": ["cargo", "xtask", "check"],
            "compile": ["cargo", "check", "--locked", "--offline", "--workspace", "--all-targets", "--all-features"],
            "navigation-build": ["python3", "/harness/install_navigation.py"]}
COMMANDS.update({
    "probe-checker-build": ["cargo", "build", "--locked", "--offline", "--release", "--package", "hsearch-object-probe"],
    "rustfs-capabilities-before": ["python3", "/harness/rustfs_probe.py", "before-restart"],
    "rustfs-capabilities-after": ["python3", "/harness/rustfs_probe.py", "after-restart"],
    "probe-history-check": ["/probe-checker/release/hsearch-object-probe"],
})
PROBE_CLIENT = {"rustfs-capabilities-before", "rustfs-capabilities-after"}
PROBE_PROFILE = PROBE_CLIENT | {"probe-checker-build", "probe-history-check"}


def writable_paths(mode):
    if mode in PROBE_CLIENT or mode == "probe-history-check":
        return ("/tmp",)
    if mode == "probe-checker-build":
        return ("/cargo", "/target", "/tmp")
    if mode == "navigation-build":
        return ("/cargo", "/target", "/navigation-output")
    if mode == "coverage":
        return ("/cargo", "/target", "/coverage-evidence")
    return ("/cargo", "/target")


def verify_checker():
    path = Path("/probe-checker/release/hsearch-object-probe")
    before = path.stat()
    expected = os.environ.get("HSEARCH_CHECKER_SHA256", "")
    if path.resolve() != path.absolute() or not path.is_file() or not 0 < before.st_size <= 64*1024**2 or len(expected) != 64:
        raise RuntimeError("Read-only checker input is not attested")
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(65536):
            digest.update(chunk)
    after = path.stat()
    if digest.hexdigest() != expected or (before.st_ino, before.st_size, before.st_mtime_ns) != (after.st_ino, after.st_size, after.st_mtime_ns):
        raise RuntimeError("Read-only checker bytes changed")


def checker_attestation():
    """Monitor-owned build proof; never run a compiler in the client profile."""
    import platform
    path = Path("/target/release/hsearch-object-probe")
    before = path.stat()
    if path.resolve() != path.absolute() or not path.is_file() or not 0 < before.st_size <= 64*1024**2:
        raise RuntimeError("Checker output is not a bounded regular build artifact")
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(65536):
            digest.update(chunk)
    after = path.stat()
    if (before.st_ino, before.st_size, before.st_mtime_ns) != (after.st_ino, after.st_size, after.st_mtime_ns):
        raise RuntimeError("Checker build artifact changed while attesting")
    config_path = Path("/probe-input/config.json")
    if config_path.stat().st_size > 65536:
        raise RuntimeError("Checker source admission exceeds bound")
    config = json.loads(config_path.read_bytes())
    toolchain = subprocess.check_output(["rustc", "--version"], timeout=10).decode().strip()
    if not toolchain.startswith("rustc 1.97.1 ") or platform.machine() not in ("x86_64", "aarch64"):
        raise RuntimeError("Checker toolchain/architecture does not match the pinned profile")
    return {"sha256": digest.hexdigest(), "bytes": before.st_size, "toolchain": toolchain,
            "architecture": platform.machine(), "source_sha256": config["source_sha256"],
            "lock_sha256": hashlib.sha256(Path("/workspace/Cargo.lock").read_bytes()).hexdigest()}


def verify_limits(cpu_millis, memory_bytes, pids, *, limits=None, uid=None):
    uid = os.getuid() if uid is None else uid
    if uid == 0:
        raise RuntimeError("Development commands require a non-root identity")
    try:
        limits = limits if limits is not None else {name: Path("/sys/fs/cgroup", name).read_text().strip()
                  for name in ("memory.max", "memory.swap.max", "cpu.max", "pids.max")}
        memory, processes = int(limits["memory.max"]), int(limits["pids.max"])
        quota, period = (int(value) for value in limits["cpu.max"].split())
        if not (0 < memory <= memory_bytes and int(limits["memory.swap.max"]) == 0 and 0 < processes <= pids and
                quota > 0 and period > 0 and quota * 1000 <= period * cpu_millis):
            raise ValueError("effective limits exceed the declared budget")
    except (OSError, ValueError, KeyError) as error:
        raise RuntimeError("Effective cgroup caps are missing, unlimited or above budget") from error
    return limits


def usage(paths, max_entries=2_000_000):
    total, entries, seen = 0, 0, set()
    pending = [(path, True) for path in paths]
    while pending:
        directory, declared_root = pending.pop()
        try:
            entries_iter = os.scandir(directory)
        except FileNotFoundError:
            # Compilers remove discovered scratch directories during a sample.
            # A vanished declared root still means accounting is unavailable.
            if declared_root:
                raise
            continue
        with entries_iter:
            for entry in entries_iter:
                entries += 1
                if entries > max_entries:
                    raise RuntimeError("Disk accounting entry bound exceeded")
                try:
                    stat = entry.stat(follow_symlinks=False)
                    identity = (stat.st_dev, stat.st_ino)
                    if identity in seen:
                        continue
                    seen.add(identity)
                    total += max(stat.st_size, getattr(stat, "st_blocks", 0) * 512)
                    if entry.is_dir(follow_symlinks=False):
                        pending.append((entry.path, False))
                except FileNotFoundError:
                    continue
    return total


def diagnostics():
    import platform
    versions = {name: subprocess.check_output(command, text=True, timeout=10).strip()
                for name, command in {"rustc": ["rustc", "--version"], "cargo": ["cargo", "--version"],
                                      "python": ["python3", "--version"], "openssl": ["openssl", "version"]}.items()}
    if os.getuid() == 0 or not versions["rustc"].startswith("rustc 1.97.1 "):
        raise RuntimeError("Expected non-root exact bootstrap toolchain")
    caps = verify_limits(2000, 4 * 1024**3, 256)
    print(json.dumps({"uid": os.getuid(), "architecture": platform.machine(), "versions": versions, "cgroup_limits": caps}))


def run(mode, disk_bytes, timeout, output_bytes=1024**2):
    if mode in PROBE_CLIENT:
        if disk_bytes != 1024**2 or timeout > 120 or output_bytes < 768*1024+4096:
            raise RuntimeError("Fixed client profile/framing budget mismatch")
        effective = verify_limits(500, 256*1024**2, 64)
    else:
        effective = verify_limits(2000, 4 * 1024**3, 256)
    writable = writable_paths(mode)
    reserve = int(os.environ.get("HSEARCH_BACKEND_RESERVE_BYTES", "0"))
    if reserve and (reserve < 10 * 1024**3 or min(shutil.disk_usage(path).free for path in writable) < reserve):
        raise RuntimeError("Actual task volume reserve is unavailable")
    if mode == "diagnostics":
        diagnostics()
        return 0
    # Repository children must not reopen the monitor's original log descriptors
    # or ptrace its counters through /proc while sharing its unprivileged UID.
    if ctypes.CDLL(None, use_errno=True).prctl(4, 0, 0, 0, 0) != 0:  # PR_SET_DUMPABLE=0
        raise RuntimeError("Could not isolate the command monitor")
    if mode in PROBE_CLIENT or mode == "probe-history-check":
        verify_checker()
    started = time.monotonic()
    child = subprocess.Popen(COMMANDS[mode], start_new_session=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    selector = selectors.DefaultSelector()
    selector.register(child.stdout, selectors.EVENT_READ)
    peak, emitted, next_sample = 0, 0, 0
    try:
        def requested_stop(_signal, _frame):
            raise RuntimeError("Development command stopped by its owner")
        signal.signal(signal.SIGTERM, requested_stop)
        while selector.get_map() or child.poll() is None:
            if time.monotonic() >= next_sample:
                peak = max(peak, usage(writable))
                if peak > disk_bytes:
                    raise RuntimeError("Declared cache/target disk budget exceeded")
                next_sample = time.monotonic() + 0.5
            if time.monotonic() - started > timeout:
                raise RuntimeError("Development command deadline exceeded")
            for key, _ in selector.select(timeout=0.05):
                chunk = os.read(key.fd, 65536)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                emitted += len(chunk)
                if emitted > output_bytes:
                    raise RuntimeError("Declared command output budget exceeded")
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()
        child.wait(timeout=3)
        receipt = {"mode": mode, "exit": child.returncode, "elapsed_seconds": round(time.monotonic() - started, 2),
                   "peak_disk_bytes": peak, "output_bytes": emitted}
        if mode == "probe-checker-build" and child.returncode == 0:
            receipt["checker"] = checker_attestation()
        if mode in PROBE_PROFILE:
            receipt["effective_limits"] = effective
            print("\nHPMONITOR2\t" + json.dumps(receipt, sort_keys=True), flush=True)
        else:
            print(json.dumps(receipt))
        return child.returncode if child.returncode >= 0 else 1
    finally:
        selector.close()
        child.stdout.close()
        if child.poll() is None:
            # Only the process group created above, never an ambient host process.
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.wait(timeout=3)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=(*COMMANDS, "diagnostics"))
    parser.add_argument("--disk-bytes", type=int, required=True)
    parser.add_argument("--timeout", type=int, required=True)
    parser.add_argument("--output-bytes", type=int, default=1024**2)
    args = parser.parse_args()
    if not 1 <= args.disk_bytes <= 64 * 1024**3 or not 1 <= args.timeout <= 1800 or not 4096 <= args.output_bytes <= 8 * 1024**2:
        parser.error("Budget outside the supported bounded range")
    raise SystemExit(run(args.mode, args.disk_bytes, args.timeout, args.output_bytes))
