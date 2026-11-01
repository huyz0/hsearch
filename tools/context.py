#!/usr/bin/env python3
"""Retrieve bounded, source-bound context; semantic queries use owned Linux sessions."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import runpy
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TASK = runpy.run_path(str(ROOT / "tools/task.py"))
need, Refused, encoded = TASK["need"], TASK["Refused"], TASK["encoded"]
SCHEMA = json.loads((ROOT / "harness/context-request.schema.json").read_text(encoding="utf-8"))
COMMON = {"schema_version", "operation", "max_bytes", "expected_source"}
FIELDS = {"packet": set(), "stats": set(), "start": set(), "stop": set(), "doctor": set(), "compile": set(),
          "search": {"pattern", "path", "cursor", "max_items"}, "slice": {"file", "start_line", "end_line"},
          "full": {"file", "reason"}, "outline": {"file"}, "diagnostics": {"file"},
          "definition": {"file", "line", "find"}, "reference": {"file", "line", "find", "max_items", "cursor"},
          "rename-preview": {"file", "line", "find", "new_name"}}
MAX_FILE = 65536
BODY_FIELDS = {"status", "backend", "semantic", "reason", "data", "notices", "tool_identity", "cargo_check_required"}


def checked_body(body, operation):
    need(type(body) is dict and set(body).issubset(BODY_FIELDS), "Backend cannot replace host context envelope fields")
    expected = {"packet": {"metadata"}, "stats": {"accounting"}, "search": {"lexical"}, "slice": {"source"}, "full": {"source"},
                "start": {"ready", "unavailable"}, "stop": {"stopped", "unavailable"}, "doctor": {"doctor", "unavailable"},
                "compile": {"compile", "unavailable"}}
    need(body.get("status") in expected.get(operation, {"semantic", "fallback", "unavailable"}), "Backend status does not match requested operation")
    need(body["status"] == "unavailable" or type(body.get("data")) is dict, "Backend data must be an object")
    if body["status"] == "semantic":
        need(body.get("semantic") is True, "Semantic backend must declare its result type")
    if body["status"] == "fallback":
        need(body.get("semantic") is False, "Fallback cannot advertise semantic authority")
    return body


def request(value):
    TASK["validate_schema"](value, SCHEMA, "context")
    need(len(encoded(value)) <= 4096, "Context request exceeds byte bound")
    operation = value["operation"]
    need(set(value).issubset(COMMON | FIELDS[operation]), "Unsupported parameter for context operation")
    if operation in {"slice", "full", "outline", "definition", "reference", "rename-preview", "diagnostics"}:
        need("file" in value, "Context operation requires a file")
    if operation in {"search", "reference"}:
        if operation == "search":
            need("pattern" in value, "Search requires a literal pattern")
        if value.get("cursor", 0):
            need("expected_source" in value, "Search pagination requires an exact source identity")
    if operation in {"definition", "reference", "rename-preview"}:
        need("line" in value and "find" in value and value["find"].count("<|>") == 1,
             "Rust semantic location requires a line and exactly one cursor marker")
    if operation == "rename-preview":
        need("new_name" in value, "Rename preview requires new_name")
    if operation == "full":
        need(value.get("reason", "").strip(), "Full-file retrieval requires a concrete reason")
    for key in ("file", "path"):
        if key in value:
            TASK["relative_path"](value[key])
    return value


def source_file(root, name):
    name = TASK["relative_path"](name)
    path = root / name
    need(path.resolve() == path.absolute() and path.is_file(), "Context file is missing or redirected")
    need(path.stat().st_size <= MAX_FILE, "Context file exceeds retrieval limit; choose a smaller source unit")
    with path.open("rb") as source:
        content = source.read(MAX_FILE + 1)
    need(len(content) <= MAX_FILE, "Context file grew beyond retrieval limit")
    return content.decode("utf-8", errors="strict"), hashlib.sha256(content).hexdigest()


def search(root, value):
    names = list(filter(None, TASK["git"]("ls-files", "--cached", "--others", "--exclude-standard", "-z", root=root).split("\0")))
    need(len(names) <= 10000, "Context search file-count bound exceeded")
    matches, skipped, consumed = [], [], 0
    prefix, pattern = value.get("path"), value["pattern"]
    cursor, count = value.get("cursor", 0), value.get("max_items", 20)
    for name in sorted(set(names)):
        if prefix and name != prefix and not name.startswith(prefix.rstrip("/") + "/"):
            continue
        try:
            text, identity = source_file(root, name)
        except (UnicodeError, Refused):
            skipped.append(name)
            continue
        for line_number, line in enumerate(text.splitlines(), 1):
            position = line.find(pattern)
            if position < 0:
                continue
            consumed += 1
            if consumed <= cursor:
                continue
            if len(matches) == count:
                return {"matches": matches, "has_more": True, "next_cursor": cursor + len(matches), "skipped_files": skipped}
            start = max(0, position - 64)
            matches.append({"file": name, "line": line_number, "file_sha256": identity,
                            "excerpt": line[start:start + 256], "excerpt_is_slice": start > 0 or len(line) > 256})
    return {"matches": matches, "has_more": False, "next_cursor": None, "skipped_files": skipped}


def prior_context(store, claim, owner):
    result = []
    for item in store.evidence(claim, owner):
        if item["kind"] == "context":
            path = store.root / "evidence" / claim / (item["sha256"] + ".json")
            result.append(json.loads(path.read_text(encoding="utf-8")))
    return result


def lexical(root, value, reason):
    operation = value["operation"]
    if operation == "outline":
        text, identity = source_file(root, value["file"])
        declarations = []
        for number, line in enumerate(text.splitlines(), 1):
            found = re.match(r"\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(fn|struct|enum|trait|mod|type|const|static)\s+(\w+)", line)
            if found:
                declarations.append({"line": number, "kind": found[1], "name": found[2]})
            need(len(declarations) <= 200, "Lexical outline exceeds item limit; request a slice")
        data = {"file": value["file"], "file_sha256": identity, "candidate_declarations": declarations}
    elif operation in {"definition", "reference", "rename-preview"}:
        text = value["find"].split("<|>", 1)[1]
        token = re.match(r"[A-Za-z_][A-Za-z0-9_]*", text)
        need(token is not None, "Fallback requires an identifier after the cursor")
        data = search(root, {"pattern": token[0], "max_items": value.get("max_items", 20), "cursor": value.get("cursor", 0)})
        data["not_a_semantic_result_or_rename_plan"] = True
    else:
        data = {"authoritative_cargo_check_required": True}
    return {"status": "fallback", "backend": "bounded-lexical", "semantic": False, "reason": reason, "data": data}


def route(store, claim, owner, value, resource_root, *, semantic=None):
    value = request(value)
    record = store.get(claim, owner)
    need(record["phase"] not in {"allocating", "closing", "closed"}, "Context worktree lifecycle is unavailable")
    before = store.fingerprint(record)
    need(value.get("expected_source", before["digest"]) == before["digest"], "Context request has stale source identity")
    root, operation = Path(record["worktree"]), value["operation"]
    budget = value.get("max_bytes", min(32768, record["packet"]["output_budget_bytes"] // 2))
    need(budget <= min(131072, record["packet"]["output_budget_bytes"] // 2), "Context response budget exceeds task allowance")
    if operation == "packet":
        body = {"status": "metadata", "data": store.context(claim, owner)}
    elif operation == "search":
        body = {"status": "lexical", "backend": "bounded-literal", "semantic": False, "data": search(root, value)}
    elif operation in {"slice", "full"}:
        text, identity = source_file(root, value["file"])
        lines = text.splitlines(keepends=True)
        start, end = value.get("start_line", 1), value.get("end_line", min(len(lines), value.get("start_line", 1) + 39))
        if operation == "full":
            earlier = prior_context(store, claim, owner)
            need(any(item["source"]["digest"] == before["digest"] and item["request"]["operation"] in {"search", "slice", "outline"}
                     and (item["request"].get("file") == value["file"] or any(hit.get("file") == value["file"] for hit in item["body"].get("data", {}).get("matches", [])))
                     for item in earlier), "Full-file retrieval needs a recorded narrower attempt at this source")
            start, end = 1, len(lines)
        need((not lines and start == 1 and end == 0) or
             (1 <= start <= end <= len(lines) and (operation == "full" or end - start < 200)), "Invalid/overwide context line range")
        body = {"status": "source", "data": {"file": value["file"], "file_sha256": identity,
                "start_line": start, "end_line": end, "text": "".join(lines[start - 1:end]), "complete_file": start == 1 and end == len(lines)}}
    elif operation == "stats":
        previous = prior_context(store, claim, owner)
        body = {"status": "accounting", "data": {"previous_context_events": len(previous), "excludes_this_response": True,
                "canonical_request_bytes": sum(item["canonical_request_bytes"] for item in previous),
                "response_payload_bytes": sum(item["response_payload_bytes"] for item in previous),
                "scope": "versioned context request/response JSON payloads only; excludes CLI framing and other tools",
                "tokenizer": None, "token_counts_are_estimates": True,
                "estimated_request_plus_response_tokens": sum(item["estimated_request_plus_response_tokens"] for item in previous)}}
    else:
        if semantic is None:
            module = runpy.run_path(str(ROOT / "tools/navigation.py"))
            semantic = module["navigate"]
        body = semantic(store, claim, owner, value, resource_root, before)
        need(type(body) is dict and set(body).issubset(BODY_FIELDS), "Backend cannot replace host context envelope fields")
        need("data" not in body or type(body["data"]) is dict, "Backend data must be an object")
        if body.get("status") == "unavailable" and operation in {"outline", "definition", "reference", "rename-preview", "diagnostics"}:
            body = lexical(root, value, body.get("reason", "Pinned semantic session unavailable"))
        if operation == "rename-preview" and body.get("status") == "semantic":
            need(body["data"].get("applied") is False, "Rename preview cannot advertise applied edits")
        if operation == "diagnostics":
            body["cargo_check_required"] = True
    need(store.fingerprint(store.get(claim, owner))["digest"] == before["digest"], "Context changed during retrieval; result is stale")
    body = checked_body(body, operation)
    response = {**body, "schema_version": 1, "claim": claim, "source": before, "operation": operation,
                "evidence_sha256": "0" * 64, "response_payload_bytes": 0, "estimated_tokens_bytes_div4": 0, "token_count_is_estimate": True}
    for _ in range(6):
        size = len(encoded(response))
        response.update(response_payload_bytes=size, estimated_tokens_bytes_div4=(size + 3) // 4)
    need(len(encoded(response)) == response["response_payload_bytes"] <= budget, "Context result exceeds byte budget; narrow or paginate the request")
    audit = {"source": before, "request": value, "body": body, "canonical_request_bytes": len(encoded(value)),
             "response_payload_bytes": response["response_payload_bytes"], "token_counts_are_estimates": True,
             "estimated_request_plus_response_tokens": (len(encoded(value)) + 3) // 4 + response["estimated_tokens_bytes_div4"]}
    response["evidence_sha256"] = store.append(claim, owner, "context", audit)
    return response


def main():
    shared = argparse.ArgumentParser(add_help=False)
    shared.add_argument("--state-root", type=Path, default=argparse.SUPPRESS)
    shared.add_argument("--resource-state-root", type=Path, default=argparse.SUPPRESS)
    parser = argparse.ArgumentParser(description=__doc__, parents=[shared])
    parser.add_argument("group", choices=("context",))
    parser.add_argument("claim")
    parser.add_argument("--owner", required=True)
    parser.add_argument("--request", type=Path)
    args = parser.parse_args()
    try:
        store = TASK["TaskStore"](getattr(args, "state_root", Path(tempfile.gettempdir()) / "hsearch-tasks"))
        if args.request:
            need(args.request.stat().st_size <= 4096, "Context request file exceeds byte bound")
            value = json.loads(args.request.read_text(encoding="utf-8"))
            result = route(store, args.claim, args.owner, value,
                           getattr(args, "resource_state_root", Path(tempfile.gettempdir()) / "hsearch-dev-leases"))
        else:
            # Preserve the M01.03 metadata interface for existing xtask callers.
            result = store.context(args.claim, args.owner)
        print(encoded(result).decode())
        return 0
    except (Refused, OSError, ValueError, KeyError, TypeError) as error:
        print(f"REFUSED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
