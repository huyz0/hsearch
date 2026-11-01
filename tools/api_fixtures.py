#!/usr/bin/env python3
"""Check the integrity of an unqualified reference corpus, not API compatibility."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import stat
import sys

ROOT = Path(__file__).resolve().parents[1]
FILES = {"documents.json", "schema.json", "search-cases.json", "analyzer-cases.json"}
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
    info = path.lstat()
    need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= 65536, "Fixture must be a bounded regular unlinked file")
    with path.open("rb") as source:
        raw = source.read(65537)
    need(len(raw) <= 65536, "Fixture byte limit exceeded")
    value = json.loads(raw.decode("utf-8"), object_pairs_hook=pairs)
    return raw, value


def check(root=ROOT):
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
    return {"integrity": "pass", "documents": len(documents), "search_cases": len(cases), "analyzer_cases": len(analyzers), "reference_execution": "NOT-ASSESSED", "hsearch_conformance": "NOT-ASSESSED", "establishes_task_completion": False}


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
