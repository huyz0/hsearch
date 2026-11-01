#!/usr/bin/env python3
"""Versioned native LLVM file segments projected onto physical source lines.

The caller owns source/path authority, profile/tool/artifact binding and coverage
floors. This module performs no I/O or compiler invocation. Function-accumulated
summary counts are diagnostics, never physical-line denominators.
"""
from dataclasses import dataclass
import hashlib
import json
import math

PROTOCOL = {"type": "llvm.coverage.json.export", "version": "3.1.0",
            "algorithm": "llvm22-physical-line-v1"}
MAX_BYTES, MAX_FILES, MAX_SEGMENTS = 64 * 1024**2, 10000, 2_000_000
MAX_LINE, MAX_COLUMN, MAX_OUTPUT, MAX_STEPS = 2_000_000, 64 * 1024**2 + 1, 2_000_000, 8_000_000


def need(condition, message):
    if not condition:
        raise ValueError(message)


@dataclass(frozen=True)
class NativeFile:
    """Validated immutable file segments and nonauthoritative line summary."""
    filename: str
    segments: tuple
    summary_lines: tuple


@dataclass(frozen=True)
class NativeReport:
    """Validated native envelope; raw digest identifies its actual input bytes."""
    raw_sha256: str
    files: tuple


def unsigned(value, maximum):
    return type(value) is int and 0 <= value <= maximum


def decode(text):
    """Validate bounded JSON framing/types before any physical projection."""
    need(type(text) is str, "Native report must be text")
    content = text.encode("utf-8")
    need(0 < len(content) <= MAX_BYTES, "Native report byte bound")
    depth, quoted, escaped = 0, False, False
    for byte in content:
        if quoted:
            if escaped:
                escaped = False
            elif byte == 92:
                escaped = True
            elif byte == 34:
                quoted = False
        elif byte == 34:
            quoted = True
        elif byte in (91, 123):
            depth += 1
            need(depth <= 32, "Native report nesting bound")
        elif byte in (93, 125):
            depth -= 1
            need(depth >= 0, "Native report framing")
    need(depth == 0 and not quoted, "Native report framing")

    def pairs(items):
        value = {}
        for key, item in items:
            need(key not in value, "Duplicate native JSON key")
            value[key] = item
        return value

    def integer(value):
        need(len(value) <= 20 and value.isascii() and value.isdecimal(), "Native JSON integer")
        number = int(value)
        need(number <= 2**64 - 1, "Native JSON integer bound")
        return number

    def decimal(value):
        number = float(value)
        need(math.isfinite(number), "Nonfinite native JSON number")
        return number

    def constant(_value):
        raise ValueError("Nonfinite native JSON constant")

    try:
        document = json.loads(text, object_pairs_hook=pairs, parse_int=integer,
                              parse_float=decimal, parse_constant=constant)
    except (json.JSONDecodeError, RecursionError) as error:
        raise ValueError("Invalid native JSON") from error
    need(type(document) is dict and set(document) == {"data", "type", "version"} and
         document["type"] == PROTOCOL["type"] and document["version"] == PROTOCOL["version"],
         "Unsupported native report envelope/version")
    need(type(document["data"]) is list and len(document["data"]) == 1,
         "Native report export count")
    export = document["data"][0]
    need(type(export) is dict and set(export) == {"files", "functions", "totals"} and
         type(export["functions"]) is list and type(export["totals"]) is dict,
         "Native export shape")
    need(type(export["files"]) is list and len(export["files"]) <= MAX_FILES, "Native file count bound")
    names, files, segment_count = set(), [], 0
    for file in export["files"]:
        need(type(file) is dict and set(file) == {"branches", "expansions", "filename", "mcdc_records", "segments", "summary"},
             "Native file shape")
        name = file["filename"]
        need(type(name) is str and 0 < len(name.encode("utf-8")) <= 4096 and "\0" not in name and
             name not in names, "Invalid/duplicate native filename")
        names.add(name)
        need(all(type(file[key]) is list for key in ("branches", "expansions", "mcdc_records", "segments")) and
             type(file["summary"]) is dict, "Native file metadata shape")
        summary = file["summary"].get("lines")
        need(type(summary) is dict and set(summary) == {"count", "covered", "percent"} and
             unsigned(summary["count"], 2**64 - 1) and unsigned(summary["covered"], summary["count"]) and
             type(summary["percent"]) in (int, float) and 0 <= summary["percent"] <= 100,
             "Native diagnostic line summary")
        previous, segments = (0, 0), []
        segment_count += len(file["segments"])
        need(segment_count <= MAX_SEGMENTS, "Native segment count bound")
        for row in file["segments"]:
            need(type(row) is list and len(row) == 6 and
                 unsigned(row[0], MAX_LINE) and row[0] > 0 and
                 unsigned(row[1], MAX_COLUMN) and row[1] > 0 and
                 unsigned(row[2], 2**63 - 1) and all(type(flag) is bool for flag in row[3:]),
                 "Native segment shape/type/range")
            need(row[3] or row[2] == 0, "Uncounted native segment has a counter")
            need((row[0], row[1]) > previous, "Duplicate/unordered native segment")
            previous = row[0], row[1]
            segments.append(tuple(row))
        files.append(NativeFile(name, tuple(segments), (summary["count"], summary["covered"], summary["percent"])))
    return NativeReport(hashlib.sha256(content).hexdigest(), tuple(files))


def project(report, selected):
    """Project selected validated files using pinned LLVM22 file-line semantics.

    Work is bounded across the entire projection. Unmapped gaps jump directly to
    the next segment; counted spans emit physical coordinates individually.
    The caller must still check coordinates against actual immutable source bytes.
    """
    need(type(report) is NativeReport and type(selected) in (set, frozenset) and
         selected.issubset({file.filename for file in report.files}), "Native projection selection")
    result, steps, emitted = {}, 0, 0
    for file in report.files:
        if file.filename not in selected:
            continue
        lines, position, wrapped = {}, 0, None
        segments = file.segments
        line = segments[0][0] if segments else 1
        while position < len(segments):
            steps += 1
            need(steps <= MAX_STEPS, "Native projection work bound")
            first = position
            entry, counted, entry_max = False, False, 0
            while position < len(segments) and segments[position][0] == line:
                row = segments[position]
                entry |= row[3] and row[4]
                if row[3] and row[4] and not row[5]:
                    counted = True
                    entry_max = max(entry_max, row[2])
                position += 1
                steps += 1
                need(steps <= MAX_STEPS, "Native projection work bound")
            skipped = position > first and not segments[first][3] and segments[first][4]
            mapped = (not skipped and ((wrapped is not None and wrapped[3]) or counted)) or entry
            if mapped:
                emitted += 1
                need(emitted <= MAX_OUTPUT, "Native physical output bound")
                lines[line] = max(wrapped[2] if wrapped else 0, entry_max) if counted else (wrapped[2] if wrapped else 0)
            if position > first:
                wrapped = segments[position - 1]
            line += 1
            if position < len(segments) and (wrapped is None or not wrapped[3]):
                line = max(line, segments[position][0])
        result[file.filename] = lines
    return result
