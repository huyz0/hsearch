#!/usr/bin/env python3
"""Run known development checks within the surrounding container's resource caps."""
import argparse
import ctypes
import json
import os
from pathlib import Path
import signal
import selectors
import shutil
import subprocess
import sys
import time

COMMANDS = {"prefetch": ["cargo", "fetch", "--locked"], "check": ["cargo", "xtask", "check"],
            "compile": ["cargo", "check", "--locked", "--offline", "--workspace", "--all-targets", "--all-features"],
            "navigation-build": ["python3", "/harness/install_navigation.py"]}


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
    pending = list(paths)
    while pending:
        directory = pending.pop()
        with os.scandir(directory) as entries_iter:
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
                        pending.append(entry.path)
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
    caps = verify_limits(2000, 4 * 1024**3, 512)
    print(json.dumps({"uid": os.getuid(), "architecture": platform.machine(), "versions": versions, "cgroup_limits": caps}))


def run(mode, disk_bytes, timeout, output_bytes=8 * 1024**2):
    verify_limits(2000, 4 * 1024**3, 512)
    reserve = int(os.environ.get("HSEARCH_BACKEND_RESERVE_BYTES", "0"))
    if reserve and (reserve < 10 * 1024**3 or min(shutil.disk_usage(path).free for path in ("/cargo", "/target")) < reserve):
        raise RuntimeError("Actual task volume reserve is unavailable")
    if mode == "diagnostics":
        diagnostics()
        return 0
    # Repository children must not reopen the monitor's original log descriptors
    # or ptrace its counters through /proc while sharing its unprivileged UID.
    if ctypes.CDLL(None, use_errno=True).prctl(4, 0, 0, 0, 0) != 0:  # PR_SET_DUMPABLE=0
        raise RuntimeError("Could not isolate the command monitor")
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
                peak = max(peak, usage(("/cargo", "/target")))
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
        print(json.dumps({"mode": mode, "exit": child.returncode, "elapsed_seconds": round(time.monotonic() - started, 2),
                          "peak_disk_bytes": peak, "output_bytes": emitted}))
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
    parser.add_argument("--output-bytes", type=int, default=8 * 1024**2)
    args = parser.parse_args()
    if not 1 <= args.disk_bytes <= 64 * 1024**3 or not 1 <= args.timeout <= 1800 or not 4096 <= args.output_bytes <= 8 * 1024**2:
        parser.error("Budget outside the supported bounded range")
    raise SystemExit(run(args.mode, args.disk_bytes, args.timeout, args.output_bytes))
