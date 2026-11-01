"""Bound immutable coverage inputs before LLVM merge or threshold evaluation.

These are trusted-coordinator content bindings, not signer authentication. The
native collector must derive expected context from current source/tools itself.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import stat


def need(condition, message):
    if not condition:
        raise ValueError(message)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()


def context_digest(context):
    need(type(context) is dict and context and len(canonical(context)) <= 4 * 1024**2, "Missing or oversized coverage context")
    return hashlib.sha256(canonical(context)).hexdigest()


def relative(value):
    need(type(value) is str and value and len(value.encode()) <= 4096 and ":" not in value and "\\" not in value and
         all(ord(char) >= 32 for char in value) and all(part not in {"", ".", ".."} for part in value.split("/")), "Invalid coverage input path")
    return value


def file_record(root, name, *, max_bytes=512 * 1024**2, single_link=False):
    root = Path(root).absolute()
    need(root.resolve() == root and root.is_dir(), "Coverage context root is redirected or absent")
    path = root / relative(name)
    need(path.is_absolute() and path.resolve() == path and path.is_relative_to(root), "Coverage input is redirected or escaped")
    before = path.stat()
    need(stat.S_ISREG(before.st_mode) and (not single_link or before.st_nlink == 1) and before.st_size <= max_bytes,
         "Coverage input is not a bounded regular owned file")
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0))
    digest, size = hashlib.sha256(), 0
    with os.fdopen(descriptor, "rb") as source:
        opened = os.fstat(source.fileno())
        need(stat.S_ISREG(opened.st_mode) and (opened.st_dev, opened.st_ino) == (before.st_dev, before.st_ino), "Coverage input identity changed")
        while chunk := source.read(65536):
            size += len(chunk)
            need(size <= max_bytes and size <= before.st_size, "Coverage input grew beyond its bound")
            digest.update(chunk)
    after = path.stat()
    need(size == before.st_size and (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
         (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns), "Coverage input changed while hashing")
    return {"path": name, "bytes": size, "sha256": digest.hexdigest()}


def check_record(root, record, *, max_bytes=512 * 1024**2, single_link=False):
    need(type(record) is dict and set(record) == {"path", "bytes", "sha256"} and type(record["bytes"]) is int and
         0 <= record["bytes"] <= max_bytes and type(record["sha256"]) is str and re.fullmatch(r"[0-9a-f]{64}", record["sha256"]), "Invalid sealed input record")
    need(file_record(root, record["path"], max_bytes=max_bytes, single_link=single_link) == record, "Coverage input hash/size is stale")


def validate_shards(root, expected_context, shards):
    """Return only recorded complete compatible profiles; never glob into merge.

    Every recorded binary/object is rehashed, including zero-test binaries. Raw
    profiles have a per-binary exclusive directory, exact inventory and one link.
    Empty raw data is allowed only for an explicitly inventoried zero-test binary.
    """
    digest = context_digest(expected_context)
    binaries, objects = expected_context["binaries"], expected_context["production_objects"]
    need(type(binaries) is dict and 0 < len(binaries) <= 1000 and type(objects) is list and len(objects) <= 10000,
         "Missing or oversized compiler artifact inventory")
    need(type(shards) is list and len(shards) == len(binaries), "Missing or duplicate coverage shard")
    total_bytes = 0
    def checked(record, *, max_bytes=512 * 1024**2, single_link=False):
        nonlocal total_bytes
        need(type(record) is dict and type(record.get("bytes")) is int and 0 <= record["bytes"] <= max_bytes,
             "Invalid coverage input size")
        total_bytes += record["bytes"]
        need(total_bytes <= 8 * 1024**3, "Coverage sealed-input aggregate byte bound exceeded")
        check_record(root, record, max_bytes=max_bytes, single_link=single_link)

    for binary_id, binary in binaries.items():
        need(type(binary_id) is str and 0 < len(binary_id) <= 512 and type(binary) is dict and
             set(binary) == {"artifact", "test_count"} and type(binary["test_count"]) is int and 0 <= binary["test_count"] <= 20000,
             "Invalid test binary inventory")
        checked(binary["artifact"])
    for record in objects:
        checked(record)
    seen, directories, profile_paths, all_profiles = set(), set(), set(), []
    for shard in shards:
        need(type(shard) is dict and set(shard) == {"context_sha256", "context", "binary_id", "completed", "command_exit", "profile_directory", "raw_profiles"}, "Invalid coverage shard shape")
        need(shard["context"] == expected_context and shard["context_sha256"] == digest and
             context_digest(shard["context"]) == digest, "Stale/incompatible source, toolchain, flags, features, target or profile")
        binary_id = shard["binary_id"]
        need(type(binary_id) is str and binary_id in binaries and binary_id not in seen, "Unexpected or duplicate test binary shard")
        seen.add(binary_id)
        need(shard["completed"] is True and type(shard["command_exit"]) is int and shard["command_exit"] == 0,
             "Unfinished or failed test command cannot contribute coverage")
        records = shard["raw_profiles"]
        need(type(records) is list and len(records) <= 20000 and (records or binaries[binary_id]["test_count"] == 0),
             "Missing instrumentation/profile for a nonempty test binary")
        directory = Path(root).absolute() / relative(shard["profile_directory"])
        need(directory.resolve() == directory and directory.is_relative_to(Path(root).absolute()) and directory.is_dir(), "Raw profile directory is absent or redirected")
        need(directory not in directories, "Raw profile directory is shared by test binaries")
        directories.add(directory)
        actual = []
        with os.scandir(directory) as entries:
            for entry in entries:
                need(len(actual) < 20000 and entry.is_file(follow_symlinks=False), "Unexpected raw profile directory member")
                actual.append(entry.name)
        expected = []
        for record in records:
            checked(record, max_bytes=16 * 1024**2, single_link=True)
            path = Path(root).absolute() / record["path"]
            need(path.parent == directory and path.suffix == ".profraw" and record["bytes"] > 0 and record["path"] not in profile_paths,
                 "Raw profile owner/type/inventory mismatch")
            profile_paths.add(record["path"])
            need(len(profile_paths) <= 20000, "Coverage raw-profile aggregate count exceeds bound")
            expected.append(path.name)
            all_profiles.append(path)
        need(sorted(actual) == sorted(expected), "Unrecorded or missing raw profile; preserve stale evidence")
    need(seen == set(binaries), "Incomplete test binary matrix")
    return all_profiles
