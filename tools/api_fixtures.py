#!/usr/bin/env python3
"""Check the integrity of an unqualified reference corpus, not API compatibility."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys

ROOT = Path(__file__).resolve().parents[1]
FILES = {"documents.json", "schema.json", "search-cases.json", "analyzer-cases.json", "mutation-cases.json", "profile-cases.json"}
SHA = re.compile(r"[a-f0-9]{64}\Z")
REVISION = re.compile(r"[a-f0-9]{40}\Z")


def need(condition, message):
    if not condition:
        raise ValueError(message)


def pairs(items):
    result = {}
    for key, value in items:
        need(key not in result, "Duplicate JSON key")
        result[key] = value
    return result


def read(path, root):
    need(path.resolve().is_relative_to(root.resolve()), "Fixture path leaves the checkout")
    need(stat.S_ISREG(path.lstat().st_mode), "Fixture path must be a regular file")
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0))
    try:
        info = os.fstat(descriptor)
        need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= 65536, "Fixture must be a bounded regular unlinked file")
        chunks, count = [], 0
        while count < 65537:
            chunk = os.read(descriptor, 65537 - count)
            if not chunk:
                break
            chunks.append(chunk)
            count += len(chunk)
        raw = b"".join(chunks)
        need(len(raw) <= 65536 and len(raw) == info.st_size, "Fixture byte limit/identity changed")
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=pairs)
        return raw, value
    finally:
        os.close(descriptor)


def checked_inputs(root=ROOT):
    root = Path(root)
    folder = root / "fixtures/api/core-v1"
    _, manifest = read(folder / "manifest.json", root)
    need(set(manifest) == {"schema_version", "status", "hsearch_runtime_implemented", "reference", "profile_sha256", "files", "missing", "constraints"}, "Unknown/missing inventory metadata")
    need(manifest["schema_version"] == 1 and manifest["hsearch_runtime_implemented"] is False, "This corpus cannot claim an implemented hsearch runtime")
    need(set(manifest["reference"]) == {"server_version", "server_revision", "client_version", "client_revision", "image_index", "linux_amd64", "linux_arm64", "source_license"}, "Static reference metadata cannot claim execution")
    need(manifest["status"] == "fixture inventory; execution evidence is recorded separately", "Inventory must distinguish inputs from observations")
    need(type(manifest["files"]) is dict and set(manifest["files"]) == FILES, "Exact corpus file inventory required")
    need(manifest["missing"] and manifest["constraints"], "Missing workloads and limitations must remain visible")
    for key in ("server_revision", "client_revision"):
        need(REVISION.fullmatch(manifest["reference"][key]) is not None, "Immutable reference revision required")
    for key in ("image_index", "linux_amd64", "linux_arm64"):
        need(re.fullmatch(r"sha256:[a-f0-9]{64}", manifest["reference"][key]) is not None, "Immutable reference image required")
    profile_raw, profile = read(root / "docs/plan/api-profile.json", root)
    need(manifest["profile_sha256"] == hashlib.sha256(profile_raw).hexdigest(), "Fixture profile binding is stale")
    areas = {item["id"]: item["kind"] for item in profile["matrix"]}
    values = {}
    for name in sorted(FILES):
        raw, value = read(folder / name, root)
        need(SHA.fullmatch(manifest["files"][name]) is not None and hashlib.sha256(raw).hexdigest() == manifest["files"][name], "Fixture hash mismatch: " + name)
        values[name] = value
    documents = values["documents.json"]
    need(type(documents) is list and 1 <= len(documents) <= 1000, "Bounded nonempty dataset required")
    ids = []
    for item in documents:
        need(set(item) == {"id", "source"} and type(item["id"]) is str and 0 < len(item["id"].encode()) <= 512, "Explicit bounded stable document ID required")
        need(item["source"]["uid"] == item["id"], "Independent sort key and document identity disagree")
        ids.append(item["id"])
    need(len(set(ids)) == len(ids), "Duplicate document identity")
    schema = values["schema.json"]
    need(schema["mappings"]["dynamic"] == "strict", "Reference schema must bound unknown field growth")
    need(schema["settings"]["number_of_shards"] == 1 and schema["settings"]["number_of_replicas"] == 0, "Reference topology changed")
    fields = set(schema["mappings"]["properties"])
    need(all(set(item["source"]) == fields for item in documents), "Dataset/schema field inventory mismatch")
    cases = values["search-cases.json"]
    need(type(cases) is list and 1 <= len(cases) <= 1000, "Bounded nonempty case inventory required")
    case_ids = set()
    for item in cases:
        identity = item["id"]
        need(type(identity) is str and re.fullmatch(r"[a-z][a-z0-9-]{0,63}", identity) and identity not in case_ids, "Duplicate/invalid case identity")
        case_ids.add(identity)
        need(item["area"] in areas and item["profile_class"] == areas[item["area"]], "Case/profile classification mismatch")
        need(set(item) == {"id", "area", "profile_class", "request", "reference_oracle"} | ({"adaptation"} if "adaptation" in item else set()), "Static cases cannot claim conformance")
        request = item["request"]
        need(set(request) == {"method", "path", "json"} and request["method"] == "POST" and request["path"] == "/{index}/_search", "Unreviewed request surface")
        need(type(request["json"]["size"]) is int and 0 <= request["json"]["size"] <= 100 and request["json"]["track_total_hits"] is True, "Reference work bounds changed")
        oracle = item["reference_oracle"]
        need(oracle["kind"] in {"hits", "aggregations"}, "Unknown oracle shape")
        if oracle["kind"] == "hits":
            need(set(oracle) == {"kind", "ids", "exact_total"} and type(oracle["ids"]) is list and set(oracle["ids"]).issubset(ids), "Hit oracle references an unknown document")
            need(len(set(oracle["ids"])) == len(oracle["ids"]) and type(oracle["exact_total"]) is int and len(oracle["ids"]) <= oracle["exact_total"] <= len(ids), "Hit oracle count/identity mismatch")
        else:
            need(set(oracle) == {"kind", "values"} and type(oracle["values"]) is dict and item.get("adaptation"), "Different aggregation semantics require a visible adaptation")
    analyzers = values["analyzer-cases.json"]
    need(type(analyzers) is list and 1 <= len(analyzers) <= 1000, "Bounded nonempty analyzer inventory required")
    analyzer_ids = set()
    for item in analyzers:
        need(item["id"] not in analyzer_ids and item["analyzer"] in schema["settings"]["analysis"]["analyzer"], "Unknown/duplicate analyzer case")
        analyzer_ids.add(item["id"])
        need(set(item) == {"id", "text", "tokens", "analyzer"}, "Static analyzer expectations cannot claim execution")
        units = len(item["text"].encode("utf-16-le")) // 2
        for position, token in enumerate(item["tokens"]):
            need(token["position"] == position and type(token["token"]) is str and token["token"], "Analyzer token/position oracle is invalid")
            need(type(token["start_offset"]) is int and type(token["end_offset"]) is int and 0 <= token["start_offset"] < token["end_offset"] <= units, "Analyzer UTF-16 offset oracle is invalid")
    mutations = values["mutation-cases.json"]
    need(type(mutations) is list and 1 <= len(mutations) <= 64, "Bounded mutation inventory required")
    references = {item["id"]: item["area"] for item in cases}
    for item in mutations:
        identity = item["id"]
        need(type(identity) is str and re.fullmatch(r"[a-z][a-z0-9-]{0,63}", identity) and identity not in references, "Duplicate/invalid reference history")
        need(set(item) == {"id", "area", "profile_class", "intent", "setup", "steps", "native_target"}, "Unknown history metadata")
        need(item["area"] in areas and item["profile_class"] == areas[item["area"]], "History/profile mismatch")
        references[identity] = item["area"]
        need(item["intent"] in {"matching_target", "intentional_difference", "outside_declared_subset"} and item["native_target"], "History adaptation must be explicit")
        need(item["setup"] in {"empty", "dataset"} and type(item["steps"]) is list and 1 <= len(item["steps"]) <= 16, "History setup/step bounds changed")
        bound = {"index"}
        for step in item["steps"]:
            need(set(step).issubset({"request", "expect", "capture"}) and {"request", "expect"}.issubset(step), "Unknown history step")
            request = step["request"]
            need(set(request).issubset({"method", "path", "json", "ndjson_lines", "raw_tail"}) and {"method", "path"}.issubset(request), "Unknown request shape")
            need(request["method"] in {"GET", "POST", "PUT", "DELETE"} and type(request["path"]) is str and request["path"].startswith("/") and len(request["path"]) <= 2048 and not any(part in request["path"] for part in ("://", "\n", "\r", "..")), "Unreviewed reference request target")
            need(not ({"json", "ndjson_lines"}.issubset(request)), "Conflicting body encodings")
            if "ndjson_lines" in request:
                need(type(request["ndjson_lines"]) is list and 1 <= len(request["ndjson_lines"]) <= 64 and all(type(line) is dict for line in request["ndjson_lines"]), "NDJSON fixture bound/type mismatch")
            need("raw_tail" not in request or "ndjson_lines" in request and type(request["raw_tail"]) is str and len(request["raw_tail"].encode()) <= 1024, "Malformed-tail fixture must be explicit and bounded")
            names = set(re.findall(r"\{([a-z][a-z0-9_]*)\}", json.dumps(request)))
            need(names.issubset(bound), "History uses an unbound response capture")
            expected = step["expect"]
            need(set(expected).issubset({"status", "fields", "nonempty_strings", "nonnegative_integers", "error_required", "hit_ids", "exact_total", "truthful_total"}) and type(expected["status"]) is int and 100 <= expected["status"] <= 599, "Unknown/bad reference oracle")
            need("error_required" not in expected or type(expected["error_required"]) is bool, "Error oracle must be boolean")
            for name in ("nonempty_strings", "nonnegative_integers"):
                need(name not in expected or type(expected[name]) is list and len(expected[name]) <= 16 and
                     all(type(path) is str and len(path) <= 512 and re.fullmatch(r"(?:/[A-Za-z0-9_]+){1,16}", path) for path in expected[name]), "Bounded typed field-oracle paths required")
            if "fields" in expected:
                need(type(expected["fields"]) is dict and len(expected["fields"]) <= 64 and
                     all(type(path) is str and len(path) <= 512 and re.fullmatch(r"(?:/[A-Za-z0-9_]+){1,16}", path) for path in expected["fields"]), "Bounded field-oracle paths required")
            need("exact_total" not in expected or type(expected["exact_total"]) is int and 0 <= expected["exact_total"] <= 10000, "Exact total oracle must be a bounded integer")
            if "truthful_total" in expected:
                total = expected["truthful_total"]
                need(set(total) == {"exact", "threshold"} and all(type(total[key]) is int for key in total) and
                     1 <= total["threshold"] <= total["exact"] <= 10000, "Truthful total must bind an independent finite ground truth")
            captures = step.get("capture", {})
            need(type(captures) is dict and len(captures) <= 8, "Capture count/type mismatch")
            for name, pointer in captures.items():
                need(re.fullmatch(r"[a-z][a-z0-9_]{0,31}", name) and name not in bound | {"namespace", "owner", "image", "container_id", "source", "head", "tree"}, "Capture cannot replace authoritative identity or prior values")
                need(type(pointer) is str and re.fullmatch(r"(?:/[A-Za-z0-9_]+){1,16}", pointer), "Bounded simple JSON pointer required")
                bound.add(name)
    profiles = values["profile-cases.json"]
    need(type(profiles) is list and len(profiles) == len(areas) and {item["area"] for item in profiles} == set(areas), "Every profile area needs one explicit native input/oracle inventory")
    for item in profiles:
        need(set(item) == {"area", "profile_class", "execution_target", "wire_abi_frozen", "native_property_input", "independent_oracle", "reference_case_ids", "unsupported_requests", "alternatives", "owning_tasks"}, "Unknown native input metadata")
        need(item["profile_class"] == areas[item["area"]] and item["execution_target"] == "future_hsearch_conformance" and item["wire_abi_frozen"] is False, "Native input cannot claim execution or a frozen wire ABI")
        need(type(item["native_property_input"]) is dict and item["native_property_input"] and type(item["independent_oracle"]) is dict and item["independent_oracle"], "Concrete independent native inputs/oracles required")
        need(item["alternatives"] and item["owning_tasks"] and all(references.get(name) == item["area"] for name in item["reference_case_ids"]), "Native/ref adaptation provenance mismatch")
        need(type(item["unsupported_requests"]) is list and len(item["unsupported_requests"]) <= 16, "Bounded unsupported-request inventory required")
        for request in item["unsupported_requests"]:
            need(set(request).issubset({"method", "path", "json"}) and request["method"] in {"GET", "POST", "PUT", "DELETE"} and request["path"].startswith("/") and len(json.dumps(request).encode()) <= 4096, "Unsupported input must remain a bounded explicit request")
    counts = {"integrity": "pass", "documents": len(documents), "search_cases": len(cases), "analyzer_cases": len(analyzers),
              "mutation_histories": len(mutations), "mutation_steps": sum(len(item["steps"]) for item in mutations),
              "profile_areas": len(profiles), "unsupported_inputs": sum(len(item["unsupported_requests"]) for item in profiles),
              "reference_execution": "NOT-ASSESSED", "hsearch_conformance": "NOT-ASSESSED", "establishes_task_completion": False}
    return counts, {"manifest": manifest, **values}


def check(root=ROOT):
    return checked_inputs(root)[0]


def main():
    argparse.ArgumentParser(description=__doc__).parse_args()
    try:
        print(json.dumps(check(), sort_keys=True))
        return 0
    except (OSError, ValueError, KeyError, TypeError, RecursionError) as error:
        print("FAIL: " + str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
