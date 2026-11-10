#!/usr/bin/env python3
"""Fixed actions inside execute.py's original capped process group.

Compile typechecks; check delegates the ordinary suite; linkage uses --no-run.
The linkage actions never invoke the produced executable. The outer monitor owns the whole wall/disk/output
deadline; short preflight timeouts consume it, never restart it. Cache hits do
not establish fresh generation, native qualification or a reproducible ELF.
"""
from contextlib import contextmanager
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import runpy
import selectors
import signal
import stat
import subprocess
import sys
import tarfile
import time
import tomllib

SOURCE = Path("/workspace")
CACHE = Path("/cargo")
CHECK = ("cargo", "check", "--locked", "--offline", "--workspace", "--all-targets", "--all-features")
ORDINARY_CHECK = ("cargo", "xtask", "check")
METADATA = ("cargo", "metadata", "--locked", "--offline", "--all-features", "--format-version", "1")
ROOTS = ("hsearch-engine", "hsearch-object", "hsearch-observe", "hsearch-types")
TREE = ("cargo", "tree", "--locked", "--offline", *[x for n in ROOTS for x in ("--package", n)],
        "--all-features", "--edges", "normal,build", "--prefix", "none", "--format", "{p} {f}")
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
PACKAGE = "protobuf-build-0.14.1"
ARCHIVE_SHA = "2df9942df2981178a930a72d442de47e2f0df18ad68e50a30f816f1848215ad0"
SELECTOR_SHA = "6226bca1f3f7ca7b06486c65389a16b8f8074a88b71bf4504480d85bdabecda6"
MANIFEST_SHA = "ce5d520efea7a16fd1765b8a06612851ea85e9a49ca401a17843a28b3ab6cd0e"
NATIVE = {
    "x86_64": ("protoc-linux-x86_64", 62, "30c6f9b5c03fb4ad7c2f0ad2efa0297c1f4e349726b6c968a029a277e864d48e"),
    "aarch64": ("protoc-linux-aarch_64", 183, "6f94b4459494b1873ad712cf5bb8e95bab72026c3be8fe95df816e989498b763"),
}
MAX_METADATA = 4 * 1024**2
MAX_OUTPUT = 512 * 1024
MAX_PROJECTION = 384 * 1024
MAX_ARCHIVE = 32 * 1024**2
MAX_EXPANDED = 64 * 1024**2
MAX_FILE = 16 * 1024**2
MAX_FILES = 2048
MAX_PACKAGES = 512
MAX_EDGES = 16384
MAX_STRING = 8192
MAX_DEPTH = 32
LINK_MODES = ("link-durability-dev", "link-durability-release")
LINK_TARGETS = {"x86_64": "x86_64-unknown-linux-gnu", "aarch64": "aarch64-unknown-linux-gnu"}
MAX_LINK_LINE = 65536
MAX_LINK_ELF = 256 * 1024**2


class Refused(RuntimeError):
    """A fixed compile precondition is unproven or over budget."""


def need(condition, reason):
    if not condition:
        raise Refused(reason)


def identity(value):
    return [value.st_dev, value.st_ino, value.st_mode, value.st_nlink,
            value.st_size, value.st_mtime_ns, value.st_ctime_ns]


def unredirected(path):
    need(path.is_absolute() and ".." not in path.parts and path.resolve(strict=True) == path,
         "redirected path")
    need(not any(p.is_symlink() for p in (path, *path.parents)), "symlink path")


@contextmanager
def regular(path, maximum):
    unredirected(path)
    before = path.lstat()
    need(stat.S_ISREG(before.st_mode) and before.st_nlink == 1 and before.st_size <= maximum,
         "nonregular, linked or oversized file")
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0))
    with os.fdopen(descriptor, "rb") as handle:
        need(identity(before) == identity(os.fstat(handle.fileno())), "file replaced on open")
        yield handle, identity(before)
        need(identity(before) == identity(os.fstat(handle.fileno())) == identity(path.lstat()),
             "file mutated or replaced")


def file_proof(path, maximum=MAX_FILE):
    digest, count, prefix = hashlib.sha256(), 0, b""
    with regular(path, maximum) as (handle, before):
        while chunk := handle.read(65536):
            count += len(chunk)
            need(count <= maximum, "file read bound")
            digest.update(chunk)
            if len(prefix) < 64:
                prefix += chunk[:64 - len(prefix)]
    return {"identity": before, "bytes": count, "sha256": digest.hexdigest(), "prefix": prefix.hex()}


def source_bytes(path, maximum):
    with regular(path, maximum) as (handle, before):
        raw = handle.read(maximum + 1)
        need(len(raw) <= maximum, "source read bound")
    return raw, {"identity": before, "sha256": hashlib.sha256(raw).hexdigest()}


class Output:
    def __init__(self):
        self.used = 0

    def write(self, raw):
        need(self.used + len(raw) <= MAX_OUTPUT - 512, "shared diagnostic budget")
        self.used += len(raw)
        sys.stdout.buffer.write(raw)
        sys.stdout.buffer.flush()

    def emit(self, value):
        raw = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode() + b"\n"
        need(len(raw) <= MAX_PROJECTION, "validation projection bound")
        self.write(raw)

    def refusal(self, reason):
        # Reserved from normal output; the enclosing monitor still counts it.
        raw = json.dumps({"compile_entry_refused": reason[:200]}, ensure_ascii=True).encode() + b"\n"
        need(self.used + len(raw) <= MAX_OUTPUT, "refusal output bound")
        sys.stdout.buffer.write(raw)
        sys.stdout.buffer.flush()


def command(argv, output, *, env=None, capture=None, timeout=None, stdout_line=None, capture_both=False):
    """No shell/session/group changes: every descendant stays with execute.py."""
    started = time.monotonic()
    child = subprocess.Popen(list(argv), cwd=SOURCE, env=env,
                             stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    selector = selectors.DefaultSelector()
    selector.register(child.stdout, selectors.EVENT_READ, "stdout")
    selector.register(child.stderr, selectors.EVENT_READ, "stderr")
    buffers = {"stdout": bytearray(), "stderr": bytearray()}
    pending = bytearray()
    try:
        while selector.get_map() or child.poll() is None:
            need(timeout is None or time.monotonic() - started <= timeout, "subcommand timeout")
            for key, _ in selector.select(.05):
                chunk = os.read(key.fd, 16384)
                if not chunk:
                    selector.unregister(key.fileobj)
                elif stdout_line is not None and key.data == "stdout":
                    pending.extend(chunk)
                    while (end := pending.find(b"\n")) >= 0:
                        need(end <= MAX_LINK_LINE, "Cargo JSON line bound")
                        stdout_line(bytes(pending[:end]))
                        del pending[:end + 1]
                    need(len(pending) <= MAX_LINK_LINE, "Cargo JSON line bound")
                elif capture is None:
                    output.write(chunk)
                else:
                    limit = capture if key.data == "stdout" or capture_both else 65536
                    need(len(buffers[key.data]) + len(chunk) <= limit, "subcommand capture bound")
                    need(not capture_both or sum(map(len, buffers.values())) + len(chunk) <= capture,
                         "combined sample capture bound")
                    buffers[key.data].extend(chunk)
        code = child.wait(timeout=1)
        if stdout_line is not None and pending:
            stdout_line(bytes(pending))
        if capture is not None and buffers["stderr"] and not capture_both:
            output.write(bytes(buffers["stderr"]))
        return code, {key: bytes(value) for key, value in buffers.items()} if capture_both else bytes(buffers["stdout"])
    finally:
        selector.close()
        child.stdout.close()
        child.stderr.close()
        if child.poll() is None:
            child.kill()
            child.wait(timeout=1)


def bounded_json(raw):
    need(len(raw) <= MAX_METADATA, "metadata byte bound")
    depth, quoted, escaped = 0, False, False
    for char in raw:
        if quoted:
            if escaped:
                escaped = False
            elif char == 92:
                escaped = True
            elif char == 34:
                quoted = False
        elif char == 34:
            quoted = True
        elif char in (91, 123):
            depth += 1
            need(depth <= MAX_DEPTH, "metadata depth bound")
        elif char in (93, 125):
            depth -= 1
            need(depth >= 0, "malformed metadata nesting")
    need(depth == 0 and not quoted, "malformed metadata framing")

    def pairs(items):
        result = {}
        for key, value in items:
            need(key not in result, "duplicate metadata key")
            result[key] = value
        return result

    def integer(value):
        need(len(value) <= 20, "metadata integer bound")
        return int(value)

    def invalid(_value):
        raise Refused("metadata noninteger number")

    value = json.loads(raw, object_pairs_hook=pairs, parse_int=integer, parse_float=invalid, parse_constant=invalid)
    pending, total = [value], 0
    while pending:
        item = pending.pop()
        total += 1
        need(total <= 131072, "metadata item bound")
        if isinstance(item, dict):
            need(len(item) <= 4096, "metadata map bound")
            pending.extend(item.keys())
            pending.extend(item.values())
        elif isinstance(item, list):
            need(len(item) <= MAX_EDGES, "metadata array bound")
            pending.extend(item)
        elif isinstance(item, str):
            need(len(item.encode()) <= MAX_STRING, "metadata string bound")
    return value


def projection(metadata, *, linkage=False):
    """Keep every field consumed by validate/inventory and cache selection."""
    need(isinstance(metadata, dict), "metadata object required")
    members = metadata["workspace_members"]
    packages, nodes = metadata["packages"], metadata["resolve"]["nodes"]
    need(0 < len(packages) <= MAX_PACKAGES and len(nodes) <= MAX_PACKAGES, "metadata package bound")
    need(sum(len(p["dependencies"]) for p in packages) + sum(len(n["deps"]) for n in nodes) <= MAX_EDGES,
         "metadata edge bound")
    need(len({p["id"] for p in packages}) == len(packages) and
         len({n["id"] for n in nodes}) == len(nodes), "duplicate package/node identity")
    result = {k: metadata[k] for k in ("workspace_root", "workspace_members", "workspace_default_members")}
    used, edges = [], 0
    for package in packages:
        row = {k: package[k] for k in ("id", "name", "version", "source", "license", "license_file", "publish", "manifest_path")}
        if linkage and package["name"] == "hsearch-testkit":
            need(type(package["targets"]) is list and len(package["targets"]) <= 64, "linkage metadata target bound")
            targets = [t for t in package["targets"] if t["name"] == "native_durability"]
            need(package["id"] in members and len(targets) == 1 and targets[0]["kind"] == ["test"] and
                 targets[0]["crate_types"] == ["bin"] and targets[0]["test"] is True and
                 package["manifest_path"] == str(SOURCE / "crates/testkit/Cargo.toml") and
                 targets[0]["src_path"] == str(SOURCE / "crates/testkit/tests/native_durability.rs"),
                 "unqualified native_durability metadata target")
            row["link_target"] = {k: targets[0][k] for k in ("name", "kind", "crate_types", "test", "src_path")}
        row["dependencies"] = []
        if package["id"] in members:
            for dependency in package["dependencies"]:
                fields = ("name", "source", "req", "kind", "rename", "target", "optional", "features", "uses_default_features")
                dep = {k: dependency[k] for k in fields}
                if "path" in dependency:
                    dep["path"] = dependency["path"]
                row["dependencies"].append(dep)
                edges += 1
        used.append(row)
    projected_nodes = []
    for node in nodes:
        deps = [{k: dep[k] for k in ("name", "pkg")} for dep in node["deps"]] if node["id"] in members else []
        edges += len(deps)
        projected_nodes.append({"id": node["id"], "features": node["features"], "deps": deps})
    need(edges <= MAX_EDGES, "metadata edge bound")
    result.update(packages=used, resolve={"nodes": projected_nodes})
    need(result["workspace_root"] == str(SOURCE), "foreign metadata workspace")
    return result


def policy_inputs(output, *, linkage=False):
    raw, policy_identity = source_bytes(SOURCE / "tools/workspace.py", 65536)
    policy = {"__file__": str(SOURCE / "tools/workspace.py"), "__name__": "fixed_workspace_policy"}
    exec(compile(raw, policy["__file__"], "exec"), policy)
    need(set(policy["PRODUCTION"]) == set(ROOTS), "production roots changed")
    code, data = command(METADATA, output, capture=MAX_METADATA, timeout=60)
    need(code == 0, "metadata nonzero exit")
    used = projection(bounded_json(data), linkage=True) if linkage else projection(bounded_json(data))
    code, tree_raw = command(TREE, output, capture=131072, timeout=60)
    need(code == 0, "production tree nonzero exit")
    tree = tree_raw.decode("utf8")
    need(len(tree.splitlines()) <= MAX_EDGES, "production tree line bound")
    policy["validate"](used, tree)
    inventory_raw, inventory_identity = source_bytes(SOURCE / "docs/plan/dependencies.json", 1048576)
    expected = policy["inventory"](used)
    need(inventory_raw.decode() == json.dumps(expected, indent=2) + "\n", "stale source inventory")
    need(source_bytes(SOURCE / "tools/workspace.py", 65536)[1] == policy_identity,
         "workspace policy drift")
    return used, {"used_validation_projection": used, "metadata_raw_sha256": hashlib.sha256(data).hexdigest(),
                  "production_tree": tree, "production_tree_raw_sha256": hashlib.sha256(tree_raw).hexdigest(),
                  "workspace_policy": policy_identity, "source_inventory": inventory_identity,
                  "inventory_validation_projection": expected}


def lock_inputs():
    manifest, mp = source_bytes(SOURCE / "Cargo.toml", 65536)
    raw, lp = source_bytes(SOURCE / "Cargo.lock", 1048576)
    lock = tomllib.loads(raw.decode())
    declared = tomllib.loads(manifest.decode()).get("workspace", {}).get("dependencies", {})
    raft = any(p["name"] in ("openraft", "raft", "protobuf-build") for p in lock["package"]) or any(n in declared for n in ("openraft", "raft"))
    if raft:
        selected = [p for p in lock["package"] if p["name"] == "protobuf-build"]
        need(len(selected) == 1 and selected[0]["version"] == "0.14.1" and
             selected[0]["source"] == REGISTRY and selected[0]["checksum"] == ARCHIVE_SHA,
             "protobuf-build lock pin/checksum changed")
    return raft, {"Cargo.toml": mp, "Cargo.lock": lp}


def native_profile():
    need(platform.system() == "Linux" and platform.machine() in NATIVE, "unsupported native profile")
    return NATIVE[platform.machine()]


class Expanded:
    def __init__(self, stream):
        self.stream, self.used = stream, 0

    def read(self, count):
        need(0 <= count <= 1048576 and self.used + count <= MAX_EXPANDED, "archive expansion bound")
        raw = self.stream.read(count)
        self.used += len(raw)
        return raw


def association(metadata):
    selected = [p for p in metadata["packages"] if p["name"] == "protobuf-build"]
    need(len(selected) == 1 and selected[0]["version"] == "0.14.1" and selected[0]["source"] == REGISTRY,
         "protobuf-build metadata identity changed")
    manifest = Path(selected[0]["manifest_path"])
    unredirected(manifest)
    relative = manifest.relative_to(CACHE / "registry/src")
    need(len(relative.parts) == 3 and relative.parts[1:] == (PACKAGE, "Cargo.toml") and
         re.fullmatch(r"(?:index\.crates\.io|github\.com)-[a-f0-9]{16}", relative.parts[0]), "foreign package cache association")
    package_root = manifest.parent
    archive = CACHE / "registry/cache" / relative.parts[0] / (PACKAGE + ".crate")
    archive_proof = file_proof(archive, MAX_ARCHIVE)
    need(archive_proof["sha256"] == ARCHIVE_SHA, "archive checksum changed")
    files, count, total = {}, 0, 0
    with regular(archive, MAX_ARCHIVE) as (handle, _identity):
        with gzip.GzipFile(fileobj=handle) as gz:
            with tarfile.open(fileobj=Expanded(gz), mode="r|") as tar:
                for member in tar:
                    count += 1
                    parts = PurePosixPath(member.name).parts
                    need(count <= MAX_FILES and len(member.name) <= 1024 and len(parts) >= 1 and
                         parts[0] == PACKAGE and not any(p in ("..", ".") for p in parts) and
                         not member.name.startswith("/") and "\\" not in member.name, "archive path/entry bound")
                    need(member.isdir() or member.isfile(), "archive nonregular member")
                    if member.isdir():
                        continue
                    name = "/".join(parts[1:])
                    need(name and name not in files and 0 <= member.size <= MAX_FILE, "archive duplicate/size bound")
                    total += member.size
                    need(total <= MAX_EXPANDED, "archive total bound")
                    digest, consumed = hashlib.sha256(), 0
                    entry = tar.extractfile(member)
                    while chunk := entry.read(65536):
                        consumed += len(chunk)
                        need(consumed <= member.size, "archive member growth")
                        digest.update(chunk)
                    need(consumed == member.size, "archive member truncated")
                    proof = file_proof(package_root / name)
                    need(proof["sha256"] == digest.hexdigest() and proof["bytes"] == member.size,
                         "cache differs from authenticated archive")
                    files[name] = proof
    need("src/protobuf_impl.rs" in files and files["src/protobuf_impl.rs"]["sha256"] == SELECTOR_SHA and
         "Cargo.toml" in files and files["Cargo.toml"]["sha256"] == MANIFEST_SHA, "selector/manifest source changed")
    actual, pending, entries = set(), [package_root], 0
    while pending:
        directory = pending.pop()
        unredirected(directory)
        with os.scandir(directory) as scan:
            for entry in scan:
                entries += 1
                need(entries <= MAX_FILES, "cache entry bound")
                need(not entry.is_symlink(), "cache symlink")
                if entry.is_dir(follow_symlinks=False):
                    pending.append(Path(entry.path))
                else:
                    name = Path(entry.path).relative_to(package_root).as_posix()
                    need(entry.is_file(follow_symlinks=False), "cache nonregular entry")
                    if name in (".cargo-checksum.json", ".cargo-ok"):
                        file_proof(Path(entry.path), 262144)
                    else:
                        actual.add(name)
    need(actual == set(files), "unassociated cache source")
    binary, machine, expected = native_profile()
    tool = package_root / "bin" / binary
    proof = files.get("bin/" + binary)
    need(proof is not None and proof["sha256"] == expected, "native protoc hash changed")
    header = bytes.fromhex(proof["prefix"])
    need(len(header) == 64 and header[:7] == b"\x7fELF\x02\x01\x01" and
         int.from_bytes(header[16:18], "little") in (2, 3) and int.from_bytes(header[18:20], "little") == machine and
         proof["identity"][2] & 0o111, "native ELF architecture/mode changed")
    need(file_proof(archive, MAX_ARCHIVE) == archive_proof, "archive changed during association")
    return tool, {"archive": str(archive), "archive_proof": archive_proof,
                  "manifest": str(manifest), "cache_files": files, "elf_machine": machine,
                  "native_system": platform.system(), "native_machine": platform.machine(),
                  "tool": str(tool), "tool_expected_sha256": expected, "tool_actual_sha256": proof["sha256"]}


def environment(tool):
    result = dict(os.environ)
    need(result.get("CARGO_HOME") == str(CACHE) and result.get("CARGO_TARGET_DIR") == "/target" and
         result.get("CARGO_BUILD_JOBS") == "2", "unqualified cache/target/jobs selector")
    forbidden = {"RUSTC", "RUSTDOC", "RUSTFMT", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTC_BOOTSTRAP",
                 "RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_ENCODED_RUSTDOCFLAGS",
                 "CARGO_BUILD_TARGET", "CARGO_BUILD_RUSTC", "CARGO_BUILD_RUSTC_WRAPPER", "PROTOC_INCLUDE", "PROTOC_PATH",
                 "LD_PRELOAD", "LD_LIBRARY_PATH", "LD_AUDIT", "PYTHONPATH", "PYTHONHOME"}
    need(not any(value and (key in forbidden or key.startswith("CARGO_TARGET_") and key != "CARGO_TARGET_DIR" or
                           key.startswith("CARGO_BUILD_") and key != "CARGO_BUILD_JOBS" or
                           key.startswith(("CARGO_ALIAS_", "CARGO_PROFILE_", "CARGO_ENCODED_")))
                 for key, value in result.items()), "conflicting compiler/tool selector")
    need(not result.get("RUSTUP_TOOLCHAIN") or result["RUSTUP_TOOLCHAIN"] == "1.97.1", "unqualified toolchain selector")
    need(not result.get("PROTOC") or tool is not None and result["PROTOC"] == str(tool), "conflicting PROTOC selector")
    if tool is not None:
        result["PROTOC"] = str(tool)
    return result


def cargo_configuration():
    need(not any(p.exists() or p.is_symlink() for p in
                 (CACHE / "config", CACHE / "config.toml", SOURCE / ".cargo/config",
                  Path("/.cargo/config"), Path("/.cargo/config.toml"))), "unqualified Cargo configuration")
    raw, proof = source_bytes(SOURCE / ".cargo/config.toml", 65536)
    need(tomllib.loads(raw.decode()) == {"alias": {"xtask": "run --locked --offline --package xtask --"},
                                       "build": {"jobs": 2}}, "source Cargo selectors changed")
    return proof


def version(raw):
    need(len(raw) <= 4096, "protoc version output bound")
    match = re.fullmatch(rb"libprotoc (3)\.([0-9]{1,5})(?:\.[0-9]{1,5})?\s*", raw)
    need(match is not None and 1 <= int(match[2]) <= 32767, "incompatible protoc version")
    return raw.decode("ascii").strip()


def link_environment():
    environment(None)
    selectors = {"CC", "CXX", "AR", "AS", "LD", "CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "RANLIB",
                 "LIBCLANG_PATH", "BINDGEN_EXTRA_CLANG_ARGS", "LIBRARY_PATH", "CPATH", "C_INCLUDE_PATH",
                 "CPLUS_INCLUDE_PATH", "COMPILER_PATH", "GCC_EXEC_PREFIX", "RUSTC_LINKER", "RUST_TARGET_PATH",
                 "CMAKE", "MAKEFLAGS", "NUM_JOBS", "OUT_DIR", "OPT_LEVEL", "LLVM_PROFILE_FILE"}
    prefixes = tuple(name + "_" for name in selectors) + ("PKG_CONFIG", "HOST_", "TARGET_")
    need(not any(value and (key in selectors or key.startswith(prefixes)) for key, value in os.environ.items()),
         "conflicting native linker/compiler selector")


def link_context(mode, metadata, output):
    need(mode in LINK_MODES, "unknown fixed linkage action")
    native_profile()
    target = LINK_TARGETS[platform.machine()]
    code, raw = command(("rustc", "-vV"), output, capture=8192, timeout=10)
    need(code == 0, "rustc host probe nonzero")
    lines = [line.partition(": ") for line in raw.decode("ascii").splitlines()]
    hosts = [value for key, sep, value in lines if sep and key == "host"]
    releases = [value for key, sep, value in lines if sep and key == "release"]
    need(hosts == [target] and releases == ["1.97.1"], "unqualified native Rust host/toolchain")
    selected = [p for p in metadata["packages"] if p["name"] == "hsearch-testkit"]
    need(len(selected) == 1 and selected[0]["id"] in metadata["workspace_members"] and
         "link_target" in selected[0], "qualified linkage package/target missing")
    package = selected[0]
    nodes = [n for n in metadata["resolve"]["nodes"] if n["id"] == package["id"]]
    need(len(nodes) == 1, "linkage feature identity missing")
    argv = ("cargo", "test", "--locked", "--offline", "--package", "hsearch-testkit", "--test", "native_durability",
            "--all-features", "--target", target, *(["--release"] if mode == "link-durability-release" else []),
            "--no-run", "--message-format=json-render-diagnostics")
    return argv, {"mode": mode, "native_target": target, "rustc_verbose": raw.decode("ascii"),
                  "package_id": package["id"], "target": package["link_target"],
                  "features": sorted(nodes[0]["features"]), "elf_machine": NATIVE[platform.machine()][1]}


def link_binary(path, context):
    folder = "release" if context["mode"] == "link-durability-release" else "debug"
    need(type(path) is str and Path(path).parent == Path("/target") / context["native_target"] / folder / "deps" and
         re.fullmatch(r"native_durability-[a-f0-9]{16}", Path(path).name), "foreign linkage artifact path/profile")
    proof = file_proof(Path(path), MAX_LINK_ELF)
    header = bytes.fromhex(proof["prefix"])
    need(len(header) == 64 and header[:7] == b"\x7fELF\x02\x01\x01" and
         int.from_bytes(header[16:18], "little") in (2, 3) and
         int.from_bytes(header[18:20], "little") == context["elf_machine"] and proof["identity"][2] & 0o111,
         "linkage ELF architecture/mode changed")
    return proof


def link_command(argv, output, env, context):
    artifacts, finished, records = [], [], 0
    def consume(raw):
        nonlocal records
        need(not finished, "Cargo record after completion")
        records += 1
        need(records <= 4096 and len(raw) <= MAX_LINK_LINE, "Cargo JSON record/line bound")
        row = bounded_json(raw)
        need(type(row) is dict and row.get("reason") in
             {"compiler-artifact", "compiler-message", "build-script-executed", "build-finished", "future-incompat-report"},
             "unknown Cargo JSON record")
        if row["reason"] == "compiler-message":
            rendered = row["message"].get("rendered")
            need(rendered is None or type(rendered) is str, "Cargo diagnostic type")
            if rendered:
                output.write(rendered.encode())
        elif row["reason"] == "build-finished":
            need(not finished and type(row["success"]) is bool, "duplicate/invalid Cargo completion")
            finished.append(row["success"])
        elif row["reason"] == "compiler-artifact" and row["target"]["name"] == "native_durability":
            target = row["target"]
            need(not artifacts and row["package_id"] == context["package_id"] and
                 {k: target[k] for k in context["target"]} == context["target"], "wrong/duplicate linkage artifact target")
            profile = row["profile"]
            release = context["mode"] == "link-durability-release"
            need(profile["test"] is True and profile["opt_level"] == ("3" if release else "0") and
                 profile["debug_assertions"] is (not release) and profile["overflow_checks"] is True and
                 sorted(row["features"]) == context["features"] and type(row["fresh"]) is bool,
                 "linkage artifact profile/features changed")
            artifacts.append({k: row[k] for k in ("executable", "profile", "features", "fresh")})
    code, _raw = command(argv, output, env=env, stdout_line=consume)
    if code:
        return code, None
    need(finished == [True] and len(artifacts) == 1, "missing/failed Cargo linkage completion")
    artifact = artifacts[0]
    artifact["file_proof"] = link_binary(artifact["executable"], context)
    artifact["scope"] = "no-run artifact availability only; fresh=true is a cache hit, not fresh linking/codegen or test execution"
    return code, artifact


def checked_action(output, link_mode=None, *, ordinary_check=False, native_sample=False):
    need(sum((link_mode is not None, ordinary_check, native_sample)) <= 1, "conflicting fixed check/link action")
    raft, source = lock_inputs()
    if not raft:
        need(not native_sample, "native sample requires qualified Raft graph")
        need(link_mode is None, "linkage requires qualified Raft graph")
        return command(ORDINARY_CHECK if ordinary_check else CHECK, output)[0]
    if link_mode is not None:
        link_environment()
    environment(None)  # Reject unqualified selectors before invoking any Cargo.
    configuration = cargo_configuration()
    # No capture/parse/path caller request is used. The trusted read-only source
    # policy is executed only as a validator; it never regenerates source here.
    metadata, graph = policy_inputs(output, linkage=True) if link_mode else policy_inputs(output)
    tool, before = association(metadata)
    env = environment(tool)
    code, raw = command((str(tool), "--version"), output, env=env, capture=4096, timeout=10)
    need(code == 0, "protoc version nonzero exit")
    observed = version(raw)
    need(association(metadata)[1] == before, "tool/cache mutated during version probe")
    argv, context = link_context(link_mode, metadata, output) if link_mode else (ORDINARY_CHECK if ordinary_check else CHECK, None)
    scope = ("actual graph/tool proof; one fixed strict sample, host export verification required" if native_sample else
             "actual graph/tool proof; unchanged ordinary suite including tests/research, no full campaign qualification" if ordinary_check else
             "actual graph/tool proof; no-run linkage only, no test execution/fresh-generation claim" if link_mode else
             "actual graph/tool proof; typecheck only, no linkage/tests/fresh-generation claim")
    output.emit({"compile_entry": "validated", "source": source, "cargo_configuration": configuration, **graph,
                 "tool_association": before, "protoc_observed_version": observed,
                 "argv": None if native_sample else argv, "scope": scope, **({"linkage_context": context} if context else {}),
                 **({"action": "check"} if ordinary_check else {})})
    sample_error = None
    if native_sample:
        try:
            code = runpy.run_path(str(SOURCE / "tools/native_durability_campaign.py"))["guest_sample"](output, env, before, command)
        except (RuntimeError, OSError, ValueError, KeyError, TypeError) as error:
            sample_error, code = error, 1
        artifact = None
    else:
        code, artifact = link_command(argv, output, env, context) if context else (command(argv, output, env=env)[0], None)
    try:
        need(association(metadata)[1] == before, "tool/cache mutated during compile")
        need(lock_inputs()[1] == source, "source/lock mutated during compile")
        need(cargo_configuration() == configuration, "Cargo configuration mutated during compile")
        for name, proof in (("tools/workspace.py", graph["workspace_policy"]),
                            ("docs/plan/dependencies.json", graph["source_inventory"])):
            need(source_bytes(SOURCE / name, 1048576)[1] == proof, "validation source mutated during compile")
        if artifact is not None:
            need(link_binary(artifact["executable"], context) == artifact["file_proof"], "linkage artifact mutated after build")
    except (Refused, OSError, ValueError):
        output.emit({"compile_entry": "postcheck refused", "cargo_exit": code})
        if code:
            return code
        raise
    if sample_error is not None:
        raise sample_error
    output.emit({"compile_entry": "complete", "cargo_exit": code, "tool_postcheck": "identical",
                 "scope": "strict candidate sample; host raw verification and complete campaign remain separate" if native_sample else
                          "ordinary check outcome only; full qualification remains separate" if ordinary_check else
                          "no-run linkage only; tests/measurements unqualified" if context else
                          "typecheck only; native linkage/tests/measurements unqualified",
                 **({"action": "check"} if ordinary_check else {}),
                 **({"linkage_artifact": artifact, "linkage_context": context} if context else {})})
    return code


def compile_fixed(output):
    return checked_action(output)


def check_fixed(output):
    return checked_action(output, ordinary_check=True)


def link_fixed(output, mode):
    need(mode in LINK_MODES, "unknown fixed linkage action")
    return checked_action(output, mode)


def main():
    output = Output()
    try:
        need((len(sys.argv) == 1 or len(sys.argv) == 2 and sys.argv[1] in (*LINK_MODES, "check", "durability-sample")) and Path(__file__) == Path("/harness/compile_entry.py") and
             Path.cwd() == SOURCE and os.getpid() == os.getpgrp(),
             "fixed monitored entry required")
        if len(sys.argv) == 1:
            return compile_fixed(output)
        if sys.argv[1] == "durability-sample":
            return checked_action(output, native_sample=True)
        return check_fixed(output) if sys.argv[1] == "check" else link_fixed(output, sys.argv[1])
    except (Refused, OSError, ValueError, KeyError, TypeError, EOFError, tarfile.TarError) as error:
        output.refusal(str(error) if isinstance(error, Refused) else "unproven fixed compile input")
        # Only our group, created by execute.py. No ambient group or detach.
        if os.getpid() == os.getpgrp():
            os.killpg(os.getpgrp(), signal.SIGTERM)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
