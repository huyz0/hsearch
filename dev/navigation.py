#!/usr/bin/env python3
"""Frozen Linux semantic-session controller; no arbitrary command or write endpoint."""
import argparse
import ctypes
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import signal
import stat
import subprocess
import time

from execute import usage, verify_limits

STATE = Path("/target/navigation")
ROOT = Path("/workspace")
MAX_OUTPUT = 131072


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def read_ready():
    descriptor = os.open(STATE / "ready.json", os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
    try:
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode) or info.st_size > 32768:
            raise RuntimeError("navigation_health_limit_or_type")
        content = os.read(descriptor, 32769)
        if len(content) > 32768:
            raise RuntimeError("navigation_health_limit_or_type")
        return json.loads(content)
    finally:
        os.close(descriptor)


def digest_file(path, limit=256 * 1024**2):
    result, size = hashlib.sha256(), 0
    with path.open("rb") as source:
        while chunk := source.read(65536):
            size += len(chunk)
            if size > limit:
                raise RuntimeError("tool_file_limit")
            result.update(chunk)
    return result.hexdigest()


def bounded(args, *, timeout=45, limit=MAX_OUTPUT, env=None):
    child = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True, env=env)
    streams = selectors.DefaultSelector()
    streams.register(child.stdout, selectors.EVENT_READ, "stdout")
    streams.register(child.stderr, selectors.EVENT_READ, "stderr")
    output, count, started = {"stdout": bytearray(), "stderr": bytearray()}, 0, time.monotonic()
    try:
        while streams.get_map() or child.poll() is None:
            if time.monotonic() - started > timeout:
                raise RuntimeError("navigation_command_deadline")
            for key, _ in streams.select(timeout=0.05):
                chunk = os.read(key.fd, 16384)
                if not chunk:
                    streams.unregister(key.fileobj)
                    continue
                count += len(chunk)
                if count > limit:
                    raise RuntimeError("navigation_output_limit")
                output[key.data].extend(chunk)
        return {"exit_code": child.wait(timeout=3), **{key: bytes(value).decode("utf-8", errors="strict") for key, value in output.items()}}
    finally:
        streams.close()
        # A CLI can exit while descendants retain its output pipes. Its own
        # session/process group still needs termination on timeout or success.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        if child.poll() is None:
            child.wait(timeout=3)
        child.stdout.close()
        child.stderr.close()


def checked(args):
    result = bounded(args, timeout=20)
    if result["exit_code"]:
        raise RuntimeError("navigation_tool_unavailable")
    return result["stdout"].strip()


def doctor():
    profile = json.loads(Path("/opt/hsearch/navigation-tools.json").read_text())
    expected = os.environ["HSEARCH_EXPECTED_LSP_SHA256"]
    if not re.fullmatch(r"[a-f0-9]{64}", expected) or digest_file(Path("/usr/local/bin/lsp")) != expected:
        raise RuntimeError("navigation_binary_identity_changed")
    if checked(["lsp", "--version"]) != "lsp " + profile["lsp_cli"]["version"]:
        raise RuntimeError("navigation_version_changed")
    rustc = checked(["rustc", "--version"])
    if not rustc.startswith("rustc " + profile["rust_analyzer"]["toolchain"] + " "):
        raise RuntimeError("navigation_toolchain_changed")
    analyzer = Path(checked(["rustup", "which", "rust-analyzer"]))
    analyzer_version = checked([str(analyzer), "--version"])
    schemas = {}
    for command in profile["commands"]:
        schema = json.loads(checked(["lsp", "schema", command]))
        properties = schema.get("properties", {})
        if "file" not in properties or "project" not in properties or "output" not in properties:
            raise RuntimeError("navigation_cli_schema_changed")
        if command == "rename" and not {"new-name", "apply"}.issubset(properties):
            raise RuntimeError("navigation_rename_schema_changed")
        schemas[command] = hashlib.sha256(encoded(schema)).hexdigest()
    return {"lsp_version": "lsp " + profile["lsp_cli"]["version"], "lsp_revision": profile["lsp_cli"]["revision"],
            "lsp_sha256": expected, "rustc": rustc, "rust_analyzer": analyzer_version,
            "rust_analyzer_sha256": digest_file(analyzer), "schema_sha256": schemas,
            "profile_sha256": hashlib.sha256(encoded(profile)).hexdigest()}


def check_storage(reserve):
    import shutil
    used = usage(("/cargo", "/target"))
    free = min(shutil.disk_usage(path).free for path in ("/cargo", "/target"))
    if used > 4 * 1024**3 or free < reserve:
        raise RuntimeError("navigation_disk_budget_or_reserve")
    return {"used_bytes": used, "actual_volume_free_bytes": free}


def serve(reserve, idle, lifetime):
    verify_limits(1000, 2 * 1024**3, 256)
    check_storage(reserve)
    if ctypes.CDLL(None, use_errno=True).prctl(4, 0, 0, 0, 0) != 0:
        raise RuntimeError("navigation_monitor_isolation_unavailable")
    identity = doctor()
    STATE.mkdir(parents=True, exist_ok=True)
    home = STATE / "state"
    home.mkdir(exist_ok=True)
    if Path(os.environ["LSP_CLI_HOME"]) != home:
        raise RuntimeError("navigation_home_not_isolated")
    (home / "config.json").write_text(json.dumps({"idleTimeout": lifetime, "managerTimeout": 20,
        "daemonIdleTimeout": lifetime, "defaultMaxItems": 20, "usePathServers": True, "settleMs": 20}))
    manager = subprocess.Popen(["lsp", "--daemon"], start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    os.set_blocking(manager.stderr.fileno(), False)
    started, output_bytes, stopping = time.monotonic(), 0, False
    activity = STATE / "activity"
    activity.touch()

    def stop(_signal, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    try:
        ready_deadline = time.monotonic() + 20
        while not (home / "manager.sock").exists():
            if manager.poll() is not None or time.monotonic() > ready_deadline:
                raise RuntimeError("navigation_daemon_not_ready")
            time.sleep(0.05)
        (STATE / "ready.json").write_text(json.dumps({"source_digest": os.environ["HSEARCH_SOURCE_DIGEST"],
            "manager_pid": manager.pid, "identity": identity, "caps": verify_limits(1000, 2 * 1024**3, 256)}))
        while not stopping:
            if manager.poll() is not None:
                raise RuntimeError("navigation_daemon_exited")
            if time.monotonic() - started > lifetime or time.time() - activity.stat().st_mtime > idle:
                break
            check_storage(reserve)
            try:
                chunk = manager.stderr.read(16384)
            except BlockingIOError:
                chunk = None
            output_bytes += len(chunk or b"")
            if output_bytes > MAX_OUTPUT:
                raise RuntimeError("navigation_daemon_output_limit")
            logs = home / "logs"
            if logs.exists() and any(item.stat().st_size > MAX_OUTPUT for item in logs.iterdir() if item.is_file()):
                raise RuntimeError("navigation_daemon_log_limit")
            time.sleep(0.25)
        return 0
    finally:
        if manager.poll() is None:
            os.killpg(manager.pid, signal.SIGTERM)
            try:
                manager.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(manager.pid, signal.SIGKILL)
                manager.wait(timeout=3)
        manager.stderr.close()


def query(value):
    if len(encoded(value)) > 8192 or value["source_digest"] != os.environ["HSEARCH_SOURCE_DIGEST"]:
        raise RuntimeError("navigation_stale_request")
    ready = read_ready()
    if ready["source_digest"] != value["source_digest"] or not (STATE / "state/manager.sock").exists():
        raise RuntimeError("navigation_stale_server")
    with (STATE / "query.lock").open("a+b") as lock:
        try:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError("navigation_query_busy") from None
        (STATE / "activity").touch()
        reserve = value["reserve_bytes"]
        if type(reserve) is not int or not 10 * 1024**3 <= reserve <= 50 * 1024**3:
            raise RuntimeError("navigation_reserve_profile_invalid")
        check_storage(reserve)
        operation = value["operation"]
        if operation == "doctor":
            return {"status": "doctor", "data": ready}
        commands = {"outline": "outline", "definition": "definition", "reference": "reference",
                    "diagnostics": "diagnostics", "rename-preview": "rename"}
        if operation not in commands:
            raise RuntimeError("navigation_operation_unsupported")
        path = ROOT / value["file"]
        if not path.resolve().is_relative_to(ROOT) or path.resolve() != path.absolute() or not path.is_file() or path.suffix != ".rs" or path.stat().st_size > 65536:
            raise RuntimeError("navigation_file_outside_profile")
        args = ["lsp", commands[operation], str(path), "--project", "/workspace", "--output", "json"]
        if operation == "outline":
            args += ["--all"]
        if operation in {"definition", "reference", "rename-preview"}:
            args += ["--scope", str(value["line"]), "--find", value["find"]]
        if operation == "reference":
            args += ["--max-items", str(value.get("max_items", 20)), "--start-index", str(value.get("cursor", 0))]
        if operation == "rename-preview":
            args += ["--new-name", value["new_name"]]
        result = bounded(args, timeout=90)
        if result["exit_code"]:
            raise RuntimeError("navigation_cli_query_failed")
        data = json.loads(result["stdout"])
        if operation == "rename-preview" and data.get("applied") is not False:
            raise RuntimeError("navigation_rename_was_not_preview")
        return {"status": "semantic", "backend": "pinned-lsp-cli/rust-analyzer", "semantic": True,
                "data": data, "notices": result["stderr"], "tool_identity": ready["identity"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("serve", "query", "health"))
    parser.add_argument("value", nargs="?")
    parser.add_argument("--reserve-bytes", type=int, default=10 * 1024**3)
    parser.add_argument("--idle-seconds", type=int, default=300)
    parser.add_argument("--lifetime-seconds", type=int, default=1800)
    args = parser.parse_args()
    if not 30 <= args.idle_seconds <= 300 or not args.idle_seconds <= args.lifetime_seconds <= 1800:
        parser.error("Navigation lifetime outside profile")
    try:
        if args.mode == "serve":
            return serve(args.reserve_bytes, args.idle_seconds, args.lifetime_seconds)
        if args.mode == "health":
            path = STATE / "ready.json"
            if not path.exists():
                print('{"ready":false}')
            else:
                value = read_ready()
                value["ready"] = value["source_digest"] == os.environ["HSEARCH_SOURCE_DIGEST"]
                wire = encoded(value)
                if len(wire) > 32768:
                    raise RuntimeError("navigation_health_limit")
                print(wire.decode())
            return 0
        result = query(json.loads(args.value))
        wire = encoded(result)
        if len(wire) > MAX_OUTPUT:
            raise RuntimeError("navigation_response_limit")
        print(wire.decode(), flush=True)
        return 0
    except (OSError, ValueError, KeyError, RuntimeError) as error:
        print(json.dumps({"status": "unavailable", "reason": str(error)[:256]}), flush=True)
        return 1 if args.mode == "serve" else 0


if __name__ == "__main__":
    raise SystemExit(main())
