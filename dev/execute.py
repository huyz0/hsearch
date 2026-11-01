#!/usr/bin/env python3
"""Run known development checks within the surrounding container's resource caps."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time

COMMANDS = {"prefetch": ["cargo", "fetch", "--locked"], "check": ["cargo", "xtask", "check"]}


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
    caps = {name: Path("/sys/fs/cgroup", name).read_text().strip()
            for name in ("memory.max", "cpu.max", "pids.max")}
    print(json.dumps({"uid": os.getuid(), "architecture": platform.machine(), "versions": versions, "cgroup_limits": caps}))


def run(mode, disk_bytes, timeout):
    if mode == "diagnostics":
        diagnostics()
        return 0
    started = time.monotonic()
    child = subprocess.Popen(COMMANDS[mode], start_new_session=True)
    peak = 0
    try:
        while child.poll() is None:
            peak = max(peak, usage(("/cargo", "/target")))
            if peak > disk_bytes:
                raise RuntimeError("Declared cache/target disk budget exceeded")
            if time.monotonic() - started > timeout:
                raise RuntimeError("Development command deadline exceeded")
            time.sleep(0.5)
        print(json.dumps({"mode": mode, "exit": child.returncode, "elapsed_seconds": round(time.monotonic() - started, 2), "peak_disk_bytes": peak}))
        return child.returncode if child.returncode >= 0 else 1
    finally:
        if child.poll() is None:
            # Only the process group created above, never an ambient host process.
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=3)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=(*COMMANDS, "diagnostics"))
    parser.add_argument("--disk-bytes", type=int, required=True)
    parser.add_argument("--timeout", type=int, required=True)
    args = parser.parse_args()
    if not 1 <= args.disk_bytes <= 64 * 1024**3 or not 1 <= args.timeout <= 1800:
        parser.error("Budget outside the supported bounded range")
    raise SystemExit(run(args.mode, args.disk_bytes, args.timeout))
