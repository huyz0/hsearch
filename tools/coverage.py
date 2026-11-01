#!/usr/bin/env python3
"""Collect and merge source-bound native Rust coverage in fresh private contexts."""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import runpy
import selectors
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid

ROOT = Path(__file__).resolve().parents[1]
TASK = runpy.run_path(str(ROOT / "tools/task.py"))
INPUT = runpy.run_path(str(ROOT / "tools/coverage_inputs.py"))
GATE = runpy.run_path(str(ROOT / "tools/coverage_gate.py"))
EXECUTE = runpy.run_path(str(ROOT / "dev/execute.py"))
SOURCES = runpy.run_path(str(ROOT / "tools/coverage_sources.py"))
RUSTFLAGS = "-C instrument-coverage -C link-dead-code"
SHA = re.compile(r"[0-9a-f]{40}")
COMPILER_POLICY = ROOT / "harness/cargo-correctness.toml"
CLIPPY_ARGUMENTS = "-D__CLIPPY_HACKERY__warnings__CLIPPY_HACKERY__"  # Rust 1.97.1 driver protocol.
COMPILER_POLICY_BYTES = (b'# Explicit CLI config, shared by Cargo and nextest\'s Cargo invocations.\n'
                         b'[env]\nRUSTC_BOOTSTRAP = { value = "-1", force = true }\n'
                         b'CLIPPY_ARGS = { value = "' + CLIPPY_ARGUMENTS.encode('ascii') + b'", force = true }\n')


def need(condition, message):
    if not condition:
        raise ValueError(message)


def cargo_policy():
    need(COMPILER_POLICY.resolve() == COMPILER_POLICY and COMPILER_POLICY.is_file() and
         COMPILER_POLICY.stat().st_size == len(COMPILER_POLICY_BYTES) and
         COMPILER_POLICY.read_bytes() == COMPILER_POLICY_BYTES, "Unqualified compiler-child policy")
    return ["--config", str(COMPILER_POLICY)]


def cargo_command(args):
    return [args[0], *cargo_policy(), *args[1:]] if args and Path(str(args[0])).name == "cargo" else args


def nextest_command(tools, action, *args):
    need(action in {"list", "run"}, "Unknown qualified nextest action")
    return [tools["nextest_executable"], "nextest", action, *cargo_policy(), *args]


def correctness_command(tools, args):
    if args[:2] == ["cargo", "nextest"]:
        return nextest_command(tools, args[2], *args[3:])
    if args[:2] == ["cargo", "clippy"]:
        return [tools["cargo_clippy_executable"], "clippy", *cargo_policy(), *args[2:]]
    if args[:2] == ["cargo", "fmt"]:
        return [tools["cargo_fmt_executable"], "fmt", *args[2:]]
    return args


def ordinary_environment(tools):
    return dict(os.environ, RUSTC=tools["cargo_compiler"], RUSTDOC=tools["cargo_rustdoc"],
                RUSTFMT=tools["rustfmt_executable"], CLIPPY_ARGS=CLIPPY_ARGUMENTS,
                RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="", RUSTC_BOOTSTRAP="-1", RUSTFLAGS="", RUSTDOCFLAGS="",
                CARGO_TARGET_DIR=tools["ordinary_target"], CARGO_BUILD_BUILD_DIR=tools["ordinary_target"], CARGO_BUILD_TARGET=tools["native_target"],
                **{tools["test_executor"]["environment_key"]: tools["test_executor"]["command"]})


def qualified_tools():
    # Keep bounded probe receipts outside source, including on qualification failure.
    target = Path(os.environ.get("CARGO_TARGET_DIR", tempfile.gettempdir())).absolute()
    runner = Runner(target / ("hsearch-tool-policy-" + uuid.uuid4().hex))
    tools = tool_identity(runner)[0]
    ordinary = runner.root / "ordinary-target"
    ordinary.mkdir(mode=0o700)
    return {**tools, "ordinary_target": str(ordinary)}


def lf_records(text):
    """LF is framing; a final delimiter is not another protocol record."""
    records = text.split("\n")
    if records and records[-1] == "": records.pop()
    return records


def save(path, value):
    encoded = INPUT["canonical"](value)
    need(len(encoded) <= 4 * 1024**2, "Coverage metadata exceeds bound")
    with path.open("xb") as output:
        output.write(encoded + b"\n")


def load(path, limit=4 * 1024**2):
    # Reuse the already-qualified bounded descriptor/JSON reader, without any review-store mutation.
    reader = runpy.run_path(str(ROOT / "tools/review.py"))["bounded_json"]
    return reader(path, limit)


def source_identity():
    head = TASK["git"]("rev-parse", "HEAD", root=ROOT).strip()
    try:
        source = TASK["source_fingerprint"](ROOT, head, [])
    except (RuntimeError, ValueError):
        status = TASK["git"]("status", "--porcelain=v1", "--untracked-files=all", root=ROOT)
        print(json.dumps({"source_binding_refused": True, "bounded_git_status": status[:4096]}), flush=True)
        raise
    need(source["clean"], "Coverage requires actual clean committed source")
    return {"head": head, "tree": TASK["git"]("rev-parse", "HEAD^{tree}", root=ROOT).strip(),
            "source_digest": source["digest"], "cargo_lock_sha256": hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest()}


class Runner:
    def __init__(self, directory, *, create=True):
        self.root = Path(directory).absolute()
        need(self.root.resolve() == self.root and not self.root.is_relative_to(ROOT), "Coverage output must be unredirected and outside source")
        if create:
            self.root.mkdir(parents=True, exist_ok=False, mode=0o700)
        need(self.root.is_dir(), "Missing coverage context")
        self.logs = self.root / ("logs-" + uuid.uuid4().hex[:12])
        self.logs.mkdir(mode=0o700)
        self.started, self.number = time.monotonic(), 0
        self.env = dict(os.environ, CARGO_TARGET_DIR=str(self.root / "target"), CARGO_BUILD_JOBS="2", CARGO_INCREMENTAL="0",
                        CARGO_BUILD_BUILD_DIR=str(self.root / "target"), CLIPPY_ARGS=CLIPPY_ARGUMENTS,
                        RUSTFLAGS=RUSTFLAGS, RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="", RUSTC_BOOTSTRAP="-1",
                        CARGO_TERM_COLOR="never", DEBUGINFOD_URLS="", PYTHONDONTWRITEBYTECODE="1")
        listing = self.root / ("listing-profiles-" + uuid.uuid4().hex[:12])
        listing.mkdir()
        self.env["LLVM_PROFILE_FILE"] = str(listing / "%m-%p.profraw")

    def command(self, args, *, env=None, timeout=900, limit=64 * 1024**2, binary=False):
        need(platform.system() == "Linux", "Native coverage commands require qualified Linux")
        args = cargo_command(args)
        need(sum(len(str(arg).encode()) + 1 for arg in args) <= 1024**2, "Coverage command argument bound exceeded")
        remaining = 900 - (time.monotonic() - self.started)
        need(remaining > 0, "Whole coverage context deadline exceeded")
        deadline = time.monotonic() + min(timeout, remaining)
        self.number += 1
        label = self.logs / str(self.number)
        print(json.dumps({"coverage_command": self.number, "tool": Path(str(args[0])).name,
                          "action": str(args[1]) if len(args) > 1 else "", "state": "started"}), flush=True)
        child = subprocess.Popen(list(map(str, args)), cwd=ROOT, env=env or self.env, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        selector = selectors.DefaultSelector()
        for name, stream in (("stdout", child.stdout), ("stderr", child.stderr)):
            selector.register(stream, selectors.EVENT_READ, name)
        captured, total, next_sample = {"stdout": bytearray(), "stderr": bytearray()}, 0, 0
        try:
            while selector.get_map() or child.poll() is None:
                need(time.monotonic() < deadline, "Coverage command deadline exceeded")
                if time.monotonic() >= next_sample:
                    need(EXECUTE["usage"]([self.root]) <= 8 * 1024**3, "Coverage context disk budget exceeded")
                    next_sample = time.monotonic() + 0.5
                for key, _ in selector.select(timeout=0.05):
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    total += len(chunk)
                    need(total <= limit, "Coverage command output bound exceeded")
                    captured[key.data].extend(chunk)
            exit_code = child.wait(timeout=3)
        finally:
            primary = sys.exception()
            try:
                if primary is not None or child.poll() is None:
                    # A dead leader can still have descendants retaining its pipes.
                    # Signal its owned group before reaping that leader, then allow
                    # nextest a finite grace to clean up separately grouped tests.
                    try:
                        os.killpg(child.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                    grace = time.monotonic() + 5
                    while True:
                        child.poll()
                        try:
                            os.killpg(child.pid, 0)
                        except ProcessLookupError:
                            break
                        if time.monotonic() >= grace:
                            try:
                                os.killpg(child.pid, signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                            break
                        time.sleep(0.02)
                    child.wait(timeout=5)
            except Exception as cleanup_error:
                if primary is None:
                    raise
                primary.add_note("Coverage child cleanup uncertain: " + str(cleanup_error)[:256])
            finally:
                selector.close()
                for stream in (child.stdout, child.stderr):
                    stream.close()
            try:
                for name, data in captured.items():
                    label.with_suffix("." + name).write_bytes(data)
                save(label.with_suffix(".json"), {"args": list(map(str, args)), "exit": child.returncode,
                                                   "complete": primary is None, "captured_bytes": total})
            except Exception as log_error:
                if primary is None:
                    raise
                primary.add_note("Coverage diagnostic save failed: " + str(log_error)[:256])
        need(exit_code == 0, "Coverage command failed; retained logs=" + str(label) + "; exit=" + str(exit_code) +
             "; diagnostic=" + captured["stderr"][:1024].decode("utf-8", errors="replace"))
        print(json.dumps({"coverage_command": self.number, "state": "completed", "exit": exit_code, "captured_bytes": total}), flush=True)
        return bytes(captured["stdout"]) if binary else captured["stdout"].decode("utf-8", errors="strict")


def tool_hash(path):
    before = path.stat()
    need(stat.S_ISREG(before.st_mode) and before.st_size <= 128 * 1024**2, "Tool executable exceeds its bound")
    digest, count = hashlib.sha256(), 0
    with path.open("rb") as source:
        while chunk := source.read(65536):
            count += len(chunk)
            need(count <= before.st_size, "Tool executable changed while hashing")
            digest.update(chunk)
    after = path.stat()
    need(count == before.st_size and (before.st_ino, before.st_size, before.st_mtime_ns) ==
         (after.st_ino, after.st_size, after.st_mtime_ns), "Tool executable changed")
    return digest.hexdigest()


def bind_compiler(runner, compiler):
    """Cargo must execute the real compiler whose bytes the context records."""
    candidate = os.environ.get("RUSTC")
    if candidate:
        selected = Path(shutil.which(candidate) or candidate).resolve()
        proxy = shutil.which("rustc")
        allowed = {compiler.resolve(), *([Path(proxy).resolve()] if proxy else [])}
        need(selected in allowed, "Conflicting RUSTC compiler override")
    runner.env["RUSTC"] = str(compiler)


def tool_identity(runner):
    need("CARGO_ENCODED_RUSTFLAGS" not in os.environ, "Unqualified compiler override: CARGO_ENCODED_RUSTFLAGS")
    for name in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_ENCODED_RUSTDOCFLAGS"):
        need(not os.environ.get(name), "Unqualified compiler override: " + name)
    need(os.environ.get("RUSTC_BOOTSTRAP") in {None, "", "-1"}, "Unqualified compiler override: RUSTC_BOOTSTRAP")
    need(os.environ.get("CLIPPY_ARGS") in {None, "", CLIPPY_ARGUMENTS}, "Unqualified compiler override: CLIPPY_ARGS")
    need(not any(key.startswith("CARGO_PROFILE_") for key in os.environ), "Unqualified Cargo profile override")
    rustc = runner.command(["rustc", "-vV"], limit=8192)
    need(rustc.startswith("rustc 1.97.1 "), "Wrong coverage Rust toolchain")
    host = next(line.removeprefix("host: ") for line in rustc.split("\n") if line.startswith("host: "))
    need(host in {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"}, "Unqualified native coverage target")
    runner.env["CARGO_BUILD_TARGET"] = host
    sysroot = Path(runner.command(["rustc", "--print", "sysroot"], limit=8192).strip())
    bind_compiler(runner, sysroot / "bin/rustc")
    runner.env["RUSTDOC"] = str(sysroot / "bin/rustdoc")
    executor = Path("/usr/bin/env")
    executor_record = {"path": str(executor), "sha256": tool_hash(executor),
                       "environment_key": "CARGO_TARGET_" + host.upper().replace("-", "_") + "_RUNNER",
                       "command": str(executor) + " --"}
    runner.env[executor_record["environment_key"]] = executor_record["command"]
    llvm = sysroot / "lib/rustlib" / host / "bin"
    cargo = runner.command(["cargo", "--version"], limit=8192).strip()
    need(cargo.startswith("cargo 1.97.1 "), "Wrong coverage Cargo toolchain")
    located_nextest = shutil.which("cargo-nextest")
    need(located_nextest is not None, "Pinned nextest is unavailable")
    nextest = Path(located_nextest).resolve()
    need(nextest.is_file(), "Pinned nextest is unavailable")
    version = runner.command([str(nextest), "--version"], limit=8192).strip()
    profile = json.loads((ROOT / "dev/correctness-tools.json").read_text(encoding="utf-8"))
    need(version.split()[:2] == ["cargo-nextest", profile["nextest"]["version"]], "Wrong nextest version")
    receipt_path = nextest.parent / "correctness-attestation.json"
    if not receipt_path.exists():
        receipt_path = Path("/opt/hsearch/correctness-attestation.json")
    receipt = load(receipt_path, 4096)
    hashes = {name: tool_hash(path) for name, path in
               [("rustc", sysroot / "bin/rustc"), ("rustdoc", sysroot / "bin/rustdoc"), ("cargo", sysroot / "bin/cargo"),
                ("cargo_clippy", sysroot / "bin/cargo-clippy"), ("clippy_driver", sysroot / "bin/clippy-driver"),
                ("cargo_fmt", sysroot / "bin/cargo-fmt"), ("rustfmt", sysroot / "bin/rustfmt"),
               ("llvm_cov", llvm / "llvm-cov"), ("llvm_profdata", llvm / "llvm-profdata"), ("llvm_objdump", llvm / "llvm-objdump"), ("nextest", nextest)]}
    need(receipt["binary_sha256"] == hashes["nextest"] and receipt["profile_sha256"] == hashlib.sha256(INPUT["canonical"](profile)).hexdigest(), "Nextest receipt/source identity mismatch")
    return {"rustc": rustc, "cargo_compiler": runner.env["RUSTC"], "cargo_rustdoc": runner.env["RUSTDOC"],
            "cargo_clippy_executable": str(sysroot / "bin/cargo-clippy"), "cargo_fmt_executable": str(sysroot / "bin/cargo-fmt"),
            "rustfmt_executable": str(sysroot / "bin/rustfmt"),
            "cargo_build_directory": runner.env["CARGO_BUILD_BUILD_DIR"], "cargo_target_directory": runner.env["CARGO_TARGET_DIR"],
            "cargo": cargo, "native_target": runner.env["CARGO_BUILD_TARGET"],
            "nextest": version, "nextest_executable": str(nextest), "hashes": hashes,
            "compiler_policy_sha256": hashlib.sha256(COMPILER_POLICY_BYTES).hexdigest(),
            "compiler_environment": {"RUSTC_BOOTSTRAP": runner.env["RUSTC_BOOTSTRAP"], "CLIPPY_ARGS": runner.env["CLIPPY_ARGS"]}, "test_executor": executor_record,
            "compiler_wrappers": {name: runner.env[name] for name in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER")}}, host, llvm


def package_inventory(runner, requested):
    metadata = json.loads(runner.command(["cargo", "metadata", "--locked", "--offline", "--all-features", "--format-version=1"]))
    members = set(metadata["workspace_members"])
    packages = {p["name"]: p for p in metadata["packages"] if p["id"] in members}
    policy = load(ROOT / "harness/coverage-policy.json")
    need(policy["schema_version"] == 1 and type(policy["minimum_percent"]) is int and policy["minimum_percent"] == 90,
         "Invalid coverage floor policy")
    excluded = set(policy["development_packages"])
    need(excluded == {"xtask", "hsearch-testkit"} and excluded.issubset(packages), "Unknown development exclusion")
    production = set(packages) - excluded
    selected = sorted(requested or production)
    need(selected and len(selected) == len(set(selected)) and set(selected).issubset(production), "Unknown coverage package scope")
    root_manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    need(root_manifest["workspace"]["lints"]["rust"]["unsafe_code"] == "forbid", "Workspace unsafe policy weakened")
    for name in selected:
        package = packages[name]
        need(not any("custom-build" in target["kind"] for target in package["targets"]), "First-party generated-source coverage requires explicit reviewed ownership")
        manifest = Path(package["manifest_path"])
        need(manifest.resolve().is_relative_to(ROOT) and tomllib.loads(manifest.read_text(encoding="utf-8"))["lints"]["workspace"] is True,
             "Production crate must inherit forbidden unsafe policy")
    external = {package["id"]: package for package in metadata["packages"] if package["id"] not in members}
    need(len(external) <= 5000 and all(type(package["source"]) is str for package in external.values()),
         "Unowned external path package requires explicit coverage ownership")
    return packages, selected, policy, external


def external_source_roots(runner, packages, tools, build):
    """Only actual declared dependencies/toolchain may supply excluded maps."""
    compiler = Path(tools["cargo_compiler"])
    commit = next((line.removeprefix("commit-hash: ") for line in tools["rustc"].split("\n") if line.startswith("commit-hash: ")), "")
    need(SHA.fullmatch(commit), "Missing exact Rust source identity")
    roots = {compiler.parent.parent / "lib/rustlib/src/rust/library", Path("/rustc") / commit / "library"}
    for package in packages.values():
        roots.add(Path(package["manifest_path"]).parent)
    need(type(build) is list and len(build) <= 20000, "Invalid compiler build inventory")
    for event in build:
        if event.get("reason") == "build-script-executed" and event["package_id"] in packages:
            generated = Path(event["out_dir"])
            need(generated.is_relative_to(runner.root / "target"), "Dependency generated-source root escaped compiler context")
            roots.add(generated)
    need(len(roots) <= 10000 and all(path.is_absolute() and path.resolve() == path and
         not path.is_relative_to(ROOT) and not ROOT.is_relative_to(path) for path in roots),
         "External coverage root is redirected or overlaps first-party source")
    return sorted(map(str, roots))


def source_metadata(path, package):
    path = ROOT / INPUT["relative"](path)
    need(path.resolve() == path and path.is_relative_to(ROOT), "Production source is redirected")
    before = path.stat()
    need(stat.S_ISREG(before.st_mode) and before.st_size <= 64 * 1024**2, "Production source file bound exceeded")
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0))
    with os.fdopen(descriptor, "rb") as source:
        opened = os.fstat(source.fileno())
        need(stat.S_ISREG(opened.st_mode) and (opened.st_dev, opened.st_ino) == (before.st_dev, before.st_ino),
             "Production source identity changed")
        raw = source.read(64 * 1024**2 + 1)
    after = path.stat()
    need(len(raw) == before.st_size and (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
         (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns), "Production source changed while reading")
    lines = raw.count(b"\n") + (1 if raw and not raw.endswith(b"\n") else 0)
    need(lines <= 2_000_000, "Production physical line count exceeds bound")
    return {"crate": package, "sha256": hashlib.sha256(raw).hexdigest(), "line_count": lines}


def tracked_paths():
    records = TASK["git"]("ls-files", "-z", root=ROOT)
    need(not records or records.endswith("\0"), "Truncated tracked source inventory")
    paths = records.split("\0")[:-1] if records else []
    need(len(paths) <= 10000 and len(set(paths)) == len(paths) and all(paths),
         "Duplicate or oversized tracked source inventory")
    for path in paths: SOURCES["source_path"](path)
    return set(paths)


def proof_base(base):
    need(type(base) is str and (base == "parentless-root" or SHA.fullmatch(base)), "Exact qualified coverage base required")
    return None if base == "parentless-root" else base


def source_manifest(sources):
    return {name: {"owner": record["crate"], "sha256": record["sha256"], "line_count": record["line_count"]}
            for name, record in sources.items()}


def proof_changes(proof):
    # The complete proof still records unchanged/mode-only files explicitly.
    return {name: record["added_head_lines"] for name, record in proof["files"].items()
            if record["added_head_lines"]}


def source_proof(base, sources):
    head = TASK["git"]("rev-parse", "HEAD", root=ROOT).strip()
    return SOURCES["prove"](ROOT, head=head, base=proof_base(base), sources=source_manifest(sources))


def verify_source_proof(base, sources, proof):
    head = TASK["git"]("rev-parse", "HEAD", root=ROOT).strip()
    SOURCES["verify"](proof, ROOT, head=head, base=proof_base(base), sources=source_manifest(sources))
    return proof_changes(proof)


def changed_lines(base, sources):
    """Compatibility wrapper over immutable proof; never parse display patches."""
    return proof_changes(source_proof(base, sources))


def initial_sources(packages, selected, tracked):
    sources = {}
    for name in selected:
        package_root = Path(packages[name]["manifest_path"]).parent
        for path in tracked:
            absolute = ROOT / path
            if path.endswith(".rs") and absolute.is_relative_to(package_root / "src"):
                sources[path] = source_metadata(path, name)
        for target in packages[name]["targets"]:
            if set(target["kind"]).intersection({"lib", "bin"}):
                path = Path(target["src_path"]).relative_to(ROOT).as_posix()
                need(path in tracked, "Production entry source is untracked")
                sources[path] = source_metadata(path, name)
    need(sources, "No owned production source inventory")
    return sources


def nextest_policy():
    need(not any(name.startswith("NEXTEST_") for name in os.environ), "Unqualified nextest environment override")
    value = tomllib.loads((ROOT / ".config/nextest.toml").read_text(encoding="utf-8"))
    expected = {"profile": {"hsearch": {"retries": 0, "test-threads": 2,
                "slow-timeout": {"period": "30s", "terminate-after": 4, "grace-period": "5s", "on-timeout": "fail"},
                "global-timeout": "15m"}}}
    need(INPUT["canonical"](value) == INPUT["canonical"](expected), "Unqualified test timeout/retry/selection override")


def owned_artifact(runner, filename):
    path = Path(filename).absolute()
    need(path.resolve() == path and path.is_relative_to(runner.root), "Compiler artifact escaped private context")
    return INPUT["file_record"](runner.root, path.relative_to(runner.root).as_posix())


def normalize_report(text, sources, packages, selected, tracked, *, runtime=False, external_roots=()):
    result = {}
    owners = sorted(((Path(packages[name]["manifest_path"]).parent, name) for name in selected), key=lambda pair: len(str(pair[0])), reverse=True)
    def include(filename):
        path = Path(filename)
        need(path.is_absolute(), "Unqualified relative coverage source")
        resolved = path.resolve()
        if not resolved.is_relative_to(ROOT):
            need(any(resolved.is_relative_to(Path(root)) for root in external_roots), "Unowned external coverage source")
            return False  # Only declared dependency/toolchain roots may be excluded.
        need(resolved == path, "Coverage source path is redirected")
        relative = path.relative_to(ROOT).as_posix()
        owner = next((name for directory, name in owners if path.is_relative_to(directory)), None)
        if owner is None:
            need(runtime and any(path.is_relative_to(Path(package["manifest_path"]).parent) for package in packages.values()),
                 "Unowned first-party coverage requires explicit reviewed ownership")
            return False
        if runtime and relative not in sources:
            directory = Path(packages[owner]["manifest_path"]).parent
            test_entries = {Path(target["src_path"]) for target in packages[owner]["targets"] if target["kind"] == ["test"]}
            need(path.is_relative_to(directory / "tests") or path in test_entries,
                 "Runtime introduced an unbound production source")
            return False
        need(relative in tracked and relative.endswith(".rs"), "Generated/untracked first-party coverage requires explicit policy")
        sources.setdefault(relative, source_metadata(relative, owner))
        return True
    for filename, lines in GATE["parse_lcov"](text, include=include).items():
        relative = Path(filename).relative_to(ROOT).as_posix()
        need(relative not in result, "Duplicate normalized first-party coverage source")
        result[relative] = lines
    return result


def verify_test_inventory(inventory, packages, selected):
    suites = inventory["rust-suites"]
    need(type(suites) is dict and 0 < len(suites) <= 1000, "Missing or oversized test binary matrix")
    expected = set()
    for name in selected:
        for target in packages[name]["targets"]:
            if target["test"]:
                need(len(target["kind"]) == 1 and target["kind"][0] in {"lib", "bin", "test"}, "Unhandled required test target kind")
                expected.add((packages[name]["id"], target["name"], target["kind"][0]))
    actual = set()
    for binary_id, suite in suites.items():
        need(type(binary_id) is str and re.fullmatch(r"[A-Za-z0-9_:/-]{1,512}", binary_id) and suite["binary-id"] == binary_id and suite["status"] == "listed",
             "Invalid/unlisted test binary")
        actual.add((suite["package-id"], suite["binary-name"], suite["kind"]))
        tests = suite["testcases"]
        need(type(tests) is dict and len(tests) <= 20000 and all(value["ignored"] is False and value["filter-match"]["status"] == "matches" for value in tests.values()),
             "Skipped/ignored/unlisted required test cannot qualify coverage")
    need(len(actual) == len(suites) and actual == expected, "Missing or unexpected declared test binary")
    return suites


def collect(directory, base, profile, requested):
    runner = Runner(directory)
    nextest_policy()
    before = source_identity()
    tools, host, llvm = tool_identity(runner)
    runner.command([sys.executable, "tools/workspace.py", "check"])
    packages, selected, policy, external_packages = package_inventory(runner, requested)
    tracked = tracked_paths()
    sources = initial_sources(packages, selected, tracked)
    flags = ["--locked", "--offline", "--all-features", "--target", host,
             *[part for name in selected for part in ("--package", name)], *( ["--release"] if profile == "release" else [])]
    build = runner.command(["cargo", "build", *flags, "--message-format=json"])
    build_events = [json.loads(line) for line in lf_records(build)]
    build_inventory = runner.root / "build-inventory.json"
    save(build_inventory, build_events)
    external_roots = external_source_roots(runner, external_packages, tools, build_events)
    artifacts, found_packages = [], set()
    production_artifacts = {name: [] for name in selected}
    for value in build_events:
        if value.get("reason") == "compiler-artifact" and value["package_id"] in {packages[name]["id"] for name in selected}:
            if set(value["target"]["kind"]).intersection({"lib", "bin"}):
                need(value["profile"]["test"] is False, "Test binary cannot define the production denominator")
                found_packages.add(value["package_id"])
                files = [Path(item) for item in value["filenames"] if item.endswith(".rlib") or value.get("executable") == item]
                name = next(name for name in selected if packages[name]["id"] == value["package_id"])
                production_artifacts[name] += [owned_artifact(runner, item) for item in files]
                artifacts += files
    need(found_packages == {packages[name]["id"] for name in selected} and artifacts, "Missing production compiler artifact")
    objects_dir = runner.root / "production-objects"
    objects_dir.mkdir()
    production_objects, map_objects = [], []
    for artifact in artifacts:
        owned_artifact(runner, artifact)
        if artifact.suffix == ".rlib":
            members = lf_records(runner.command(["ar", "t", artifact], limit=1024**2))
            need(len(members) <= 10000 and all(member and "\n" not in member for member in members), "Archive member inventory bound exceeded")
            native = [member for member in members if member.endswith(".o")]
            # Native member output is emitted as a file by ar, not interpreted as a path.
            for member in native:
                output = objects_dir / (str(len(production_objects)) + ".o")
                need(re.fullmatch(r"[A-Za-z0-9_.-]{1,256}", member) and not member.startswith("-"), "Unsafe compiler archive member name")
                content = runner.command(["ar", "p", artifact, member], timeout=30, limit=512 * 1024**2, binary=True)
                with output.open("xb") as target:
                    target.write(content)
                production_objects.append(owned_artifact(runner, output))
                sections = runner.command([str(llvm / "llvm-objdump"), "--section-headers", output], limit=1024**2)
                if "__llvm_covmap" in sections or "__llvm_covfun" in sections:
                    map_objects.append(output)
        else:
            production_objects.append(owned_artifact(runner, artifact))
            sections = runner.command([str(llvm / "llvm-objdump"), "--section-headers", artifact], limit=1024**2)
            if "__llvm_covmap" in sections or "__llvm_covfun" in sections:
                map_objects.append(artifact)
    baseline = {}
    if map_objects:
        text = runner.command([str(llvm / "llvm-cov"), "export", "--empty-profile", "--num-threads=2", "--format=lcov", map_objects[0],
                               *[part for item in map_objects[1:] for part in ("--object", item)]])
        baseline = normalize_report(text, sources, packages, selected, tracked, external_roots=external_roots)
    non_executable = {name: assertion for name, assertion in policy["non_executable"].items() if name in sources}
    proof = source_proof(base, sources)
    changed = proof_changes(proof)
    # Apply denominator/missing-map checks before running tests; zero counts are intentionally not a pass here.
    GATE["evaluate"](sources, baseline, {}, changed, non_executable, selected)
    raw_inventory = runner.command(nextest_command(tools, "list", *flags, "--message-format=json"))
    inventory = json.loads(raw_inventory)
    suites = verify_test_inventory(inventory, packages, selected)
    inventory_path = runner.root / "test-inventory.json"
    save(inventory_path, inventory)
    binaries = {binary_id: {"artifact": owned_artifact(runner, suite["binary-path"]), "test_count": len(suite["testcases"])} for binary_id, suite in suites.items()}
    context = {"schema_version": 2, "source_proof": proof, "generation": uuid.uuid4().hex, "source": before, "base": base, "toolchain": tools,
               "target": host, "profile": profile, "features": "all", "rustflags": RUSTFLAGS, "packages": selected,
               "sources": sources, "baseline": {name: {str(line): hits for line, hits in lines.items()} for name, lines in baseline.items()},
               "changed": changed, "non_executable": non_executable, "binaries": binaries, "production_objects": production_objects,
               "test_inventory": owned_artifact(runner, inventory_path), "production_artifacts": production_artifacts}
    context["build_inventory"] = owned_artifact(runner, build_inventory)
    context["external_roots"] = external_roots
    need(source_identity() == before, "Source changed during compilation")
    save(runner.root / "context.json", context)
    shard_directory = runner.root / "shards"
    shard_directory.mkdir()
    profiles_root = runner.root / "profiles"
    profiles_root.mkdir()
    for index, (binary_id, binary) in enumerate(sorted(binaries.items())):
        profile_directory = profiles_root / str(index)
        profile_directory.mkdir()
        if binary["test_count"]:
            env = dict(runner.env, LLVM_PROFILE_FILE=str(profile_directory / "%m-%p.profraw"))
            runner.command(nextest_command(tools, "run", *flags, "--profile", "hsearch", "--no-tests=fail", "-E", "binary_id(=" + binary_id + ")"), env=env)
        profiles = sorted(profile_directory.iterdir())
        records = [INPUT["file_record"](runner.root, path.relative_to(runner.root).as_posix(), max_bytes=16 * 1024**2, single_link=True) for path in profiles]
        save(shard_directory / (str(index) + ".json"), {"context_sha256": INPUT["context_digest"](context), "context": context,
             "binary_id": binary_id, "completed": True, "command_exit": 0, "profile_directory": profile_directory.relative_to(runner.root).as_posix(), "raw_profiles": records})
    need(source_identity() == before, "Source changed during instrumented tests")
    return merge(runner.root)


def merge(directory):
    runner = Runner(directory, create=False)
    context = load(runner.root / "context.json")
    need(type(context["schema_version"]) is int and context["schema_version"] == 2 and context["source"] == source_identity() and context["profile"] in {"dev", "release"} and
         context["features"] == "all" and context["rustflags"] == RUSTFLAGS, "Stale or unqualified coverage collection")
    nextest_policy()
    tools, host, llvm = tool_identity(runner)
    need(context["toolchain"] == tools and context["target"] == host, "Toolchain/target changed before merge")
    packages, selected, policy, external_packages = package_inventory(runner, context["packages"])
    tracked = tracked_paths()
    required = initial_sources(packages, selected, tracked)
    need(all(context["sources"].get(name) == record for name, record in required.items()),
         "Coverage context omitted or changed a declared production source")
    owners = sorted(((Path(packages[name]["manifest_path"]).parent, name) for name in selected),
                    key=lambda item: len(str(item[0])), reverse=True)
    for name, metadata in context["sources"].items():
        path = ROOT / name
        owner = next((owner for directory, owner in owners if path.is_relative_to(directory)), None)
        need(owner is not None and name in tracked and metadata == source_metadata(name, owner),
             "Production source metadata/ownership is stale")
    need(context["changed"] == verify_source_proof(context["base"], context["sources"], context["source_proof"]),
         "Changed-line proof is stale or corrupt")
    INPUT["check_record"](runner.root, context["build_inventory"])
    external_roots = external_source_roots(runner, external_packages, tools, load(runner.root / context["build_inventory"]["path"]))
    need(context["external_roots"] == external_roots, "Dependency/toolchain source ownership changed")
    INPUT["check_record"](runner.root, context["test_inventory"])
    suites = verify_test_inventory(load(runner.root / context["test_inventory"]["path"]), packages, selected)
    need(set(suites) == set(context["binaries"]), "Coverage context omitted a declared test binary")
    for binary_id, suite in suites.items():
        binary = context["binaries"][binary_id]
        need(binary["test_count"] == len(suite["testcases"]) and binary["artifact"] == owned_artifact(runner, suite["binary-path"]), "Frozen test inventory/binary mismatch")
    need(set(context["production_artifacts"]) == set(selected), "Missing production package artifact inventory")
    for records in context["production_artifacts"].values():
        need(type(records) is list and records, "Missing production archive/binary")
        for record in records:
            INPUT["check_record"](runner.root, record)
    for name, metadata in context["sources"].items():
        need(metadata == source_metadata(name, metadata["crate"]) and metadata["crate"] in selected, "Production source metadata is stale")
    expected = {str(index) + ".json" for index in range(len(context["binaries"]))}
    shard_directory = runner.root / "shards"
    need({path.name for path in shard_directory.iterdir()} == expected, "Unexpected or incomplete shard manifest set")
    shards = [load(shard_directory / name) for name in sorted(expected, key=lambda name: int(name.split(".")[0]))]
    profiles = INPUT["validate_shards"](runner.root, context, shards)
    runtime = {}
    if profiles:
        merged = runner.root / ("merged-" + uuid.uuid4().hex[:12] + ".profdata")
        runner.command([str(llvm / "llvm-profdata"), "merge", "--sparse", "--num-threads=2", *profiles, "-o", merged])
        binary_paths = [runner.root / value["artifact"]["path"] for value in context["binaries"].values()]
        text = runner.command([str(llvm / "llvm-cov"), "export", "--num-threads=2", "--format=lcov", "--instr-profile=" + str(merged), binary_paths[0],
                               *[part for item in binary_paths[1:] for part in ("--object", item)]])
        sources = dict(context["sources"])
        tracked = tracked_paths()
        runtime = normalize_report(text, sources, packages, selected, tracked, runtime=True, external_roots=external_roots)
        need(sources == context["sources"], "Runtime introduced an unbound production source")
    map_objects = []
    for record in context["production_objects"]:
        path = runner.root / record["path"]
        sections = runner.command([str(llvm / "llvm-objdump"), "--section-headers", path], limit=1024**2)
        if "__llvm_covmap" in sections or "__llvm_covfun" in sections:
            map_objects.append(path)
    baseline = {}
    if map_objects:
        text = runner.command([str(llvm / "llvm-cov"), "export", "--empty-profile", "--num-threads=2", "--format=lcov", map_objects[0],
                               *[part for item in map_objects[1:] for part in ("--object", item)]])
        sources = dict(context["sources"])
        tracked = tracked_paths()
        baseline = normalize_report(text, sources, packages, selected, tracked, external_roots=external_roots)
        need(sources == context["sources"], "Production baseline introduced an unbound source")
    need({name: {str(line): hits for line, hits in lines.items()} for name, lines in baseline.items()} == context["baseline"], "Production denominator changed before merge")
    need(context["changed"] == verify_source_proof(context["base"], context["sources"], context["source_proof"]),
         "Changed-line proof changed during merge")
    need(context["non_executable"] == {name: value for name, value in policy["non_executable"].items() if name in context["sources"]}, "Non-executable audit changed")
    report = GATE["evaluate"](context["sources"], baseline, runtime, context["changed"], context["non_executable"], selected)
    # Recheck all physical inputs and source after LLVM reads them; leave every failed context for inspection.
    INPUT["validate_shards"](runner.root, context, shards)
    need(context["source"] == source_identity(), "Source changed during coverage merge")
    binding = {"context_schema_version": context["schema_version"], "base": context["base"],
               "source_proof_sha256": INPUT["context_digest"](context["source_proof"]),
               "compiler_policy_sha256": tools["compiler_policy_sha256"],
               "native_target": tools["native_target"],
               "private_build_directory": tools["cargo_build_directory"] == tools["cargo_target_directory"],
               "rust_external_tools": {key: tools[key] for key in ("cargo_clippy_executable", "cargo_fmt_executable", "rustfmt_executable")},
               "compiler_environment": tools["compiler_environment"], "nextest_executable": tools["nextest_executable"],
               "test_executor": tools["test_executor"],
               "compiler_wrappers": tools["compiler_wrappers"]}
    result = {"schema_version": 1, "source": context["source"], "source_binding": binding,
              "profile": context["profile"], "target": host,
              "context_sha256": INPUT["context_digest"](context), "report": report,
              "runtime_lines": {name: {str(line): hits for line, hits in lines.items()} for name, lines in runtime.items()}}
    save(runner.root / ("result-" + uuid.uuid4().hex[:12] + ".json"), result)
    print(json.dumps({"coverage": "pass" if report["passed"] else "fail", "profile": context["profile"], "target": host,
                      "head": context["source"]["head"], "source_binding": binding,
                      "aggregate": report["aggregate"], "changed": report["changed"], "crates": report["crates"]}), flush=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("run", "collect", "merge"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--base")
    parser.add_argument("--profile", choices=("dev", "release"), default="dev")
    parser.add_argument("--package", action="append", default=[])
    args = parser.parse_args()
    need(platform.system() == "Linux", "Workspace coverage collection requires qualified native Linux")
    if args.action == "merge":
        results = [merge(args.output)]
    elif args.action == "collect":
        need(args.base, "Coverage collection requires its exact base")
        results = [collect(args.output, args.base, args.profile, args.package)]
    else:
        need(args.base and args.output.resolve() == args.output.absolute() and not args.output.resolve().is_relative_to(ROOT), "Explicit external output and base required")
        args.output.mkdir(parents=True, exist_ok=False, mode=0o700)
        results = [collect(args.output / profile, args.base, profile, args.package) for profile in ("dev", "release")]
    passed = all(result["report"]["passed"] for result in results)
    if args.action == "run":
        save(args.output / "summary.json", {"passed": passed, "source": source_identity(), "profiles": results})
    return 0 if passed else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, RuntimeError, KeyError, TypeError, OSError, subprocess.SubprocessError) as error:
        print("COVERAGE REFUSED: " + str(error)[:4096], file=sys.stderr)
        raise SystemExit(1)
