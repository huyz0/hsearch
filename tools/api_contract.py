#!/usr/bin/env python3
"""Validate the design profile, API gate ordering and generated compatibility matrix."""
from __future__ import annotations
import argparse
import json
from pathlib import Path
import sys
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
PLAN = ROOT / "docs/plan"
BEGIN, END = "<!-- api-matrix:start -->", "<!-- api-matrix:end -->"
MINIMUM_ORDER = {("M02.01", "M19.01"), ("M02.05", "M02.08"), ("M03.03", "M02.08"),
                 ("M03.03", "M02.05"), ("M13.01", "M02.08"), ("M06.05", "M13.01"),
                 ("M07.04", "M07.08"), ("M17.01", "M19.01")}
REQUIRED_LIMITS = {"request_bytes", "response_bytes", "selected_partitions", "inflight_rpcs",
                   "decoded_bytes", "cpu_work", "remote_attempts", "remote_bytes", "queue_bytes",
                   "concurrent_requests_per_tenant", "active_pits_per_tenant", "global_active_pits",
                   "pin_bytes_per_tenant", "global_pin_bytes", "idempotency_retention_bytes",
                   "maintenance_debt"}


def need(condition, message):
    if not condition:
        raise ValueError(message)


def validate(profile, ledger):
    need(type(profile["schema_version"]) is int and profile["schema_version"] == 1 and profile["profile_id"] == "core-v1", "Unknown API design profile")
    need(profile["status"] == "design" and profile["runtime_implemented"] is False,
         "Design matrix cannot advertise implemented runtime support")
    need(profile["revision"] and profile["authority"] and profile["upstream_basis"], "Missing profile provenance")
    for url in profile["sources"].values():
        parsed = urlsplit(url)
        need(parsed.scheme == "https" and parsed.netloc == "docs.opensearch.org" and not parsed.query,
             "API facts require direct official OpenSearch sources")
    limits = profile["example_local_limits"]
    need(limits and all(type(value) is int and value > 0 for value in limits.values()), "API limit must be finite positive integer")
    need(limits["max_inflight_partition_rpcs"] <= limits["max_selected_partitions"], "Concurrent/total fanout limits inconsistent")
    need(limits["max_page_hits"] <= limits["max_offset_plus_size"], "Page/offset limits inconsistent")
    required = set(profile["required_profile_limits"])
    need(REQUIRED_LIMITS.issubset(required),
         "Missing required work/retention budget")
    milestones = {m["id"]: m for m in ledger["milestones"]}
    tasks = {t["id"]: t for m in ledger["milestones"] for t in m["tasks"]}
    need("M19.01" in tasks and [m["id"] for m in ledger["milestones"]][:3] == ["M00", "M19", "M01"],
         "API direction must precede remaining implementation in the execution roadmap")
    ids = set()
    for row in profile["matrix"]:
        need(row["id"] not in ids, "Duplicate API matrix ID")
        ids.add(row["id"])
        need(row["kind"] in {"subset", "difference", "unsupported"}, "Unknown API classification")
        for field in ("surface", "upstream", "contract", "scalability", "acceptance"):
            need(isinstance(row[field], str) and row[field].strip(), "Incomplete API contract/rationale/oracle")
        need(isinstance(row["alternative"], str) and bool(row["alternative"].strip()), "Difference/exclusion requires a scalable alternative")
        need(set(row["sources"]).issubset(profile["sources"]), "Unknown API source reference")
        need(row["owners"] and set(row["owners"]).issubset(tasks), "API obligation lacks owning tasks")
    need(len(ids) >= 16, "Incomplete compatibility surface inventory")
    gate_ids = set()
    for gate in profile["gates"]:
        need(gate["id"] not in gate_ids and gate["owner"] in tasks, "Duplicate/unowned scalability gate")
        gate_ids.add(gate["id"])
        need(gate["measurement"] and gate["pass"] and gate["observed"] is False,
             "Proposed scalability gate cannot be marked as measured evidence")
    need({f"G{i:02}" for i in range(1, 8)}.issubset(gate_ids), "Missing measurable qualification gates")
    scale_gate = next(gate for gate in profile["gates"] if gate["id"] == "G06")
    need(scale_gate["owner"] == "M17.03" and scale_gate.get("calibration_owner") == "M02.05",
         "Scale gate must separate early calibration from real-service qualification")

    def dependencies(task):
        return [child["id"] for ref in task["depends_on"] for child in
                (milestones[ref]["tasks"] if ref in milestones else [tasks[ref]])]

    def reaches(start, target, seen=None):
        seen = set() if seen is None else seen
        if start in seen:
            return False
        seen.add(start)
        return any(dep == target or reaches(dep, target, seen) for dep in dependencies(tasks[start]))

    constraints = {tuple(item) for item in profile["ordering_constraints"]}
    need(MINIMUM_ORDER.issubset(constraints), "Missing mandatory API/scalability ordering policy")
    for task, prerequisite in constraints:
        need(task in tasks and prerequisite in tasks and reaches(task, prerequisite),
             f"API/scalability prerequisite missing: {task} -> {prerequisite}")
    owners = {owner for row in profile["matrix"] for owner in row["owners"]}
    owners.update(gate["owner"] for gate in profile["gates"])
    need(all(reaches(owner, "M19.01") for owner in owners), "API-owning work bypasses the authoritative profile")


def table(profile):
    lines = ["| Area / target class | OpenSearch reference behavior | Hsearch target and alternative | Scalability rationale and acceptance |",
             "| --- | --- | --- | --- |"]
    for row in profile["matrix"]:
        source = ", ".join(f"[{name}]({profile['sources'][name]})" for name in row["sources"])
        upstream = row["upstream"] + (" " + source if source else "")
        target = row["contract"] + " **Alternative:** " + row["alternative"]
        proof = "**Design inference:** " + row["scalability"] + " **Test:** " + row["acceptance"] + " Owners: " + ", ".join(row["owners"]) + "."
        cells = [f"{row['id']} {row['surface']} — **{row['kind']}**", upstream, target, proof]
        need(all("|" not in cell and "\n" not in cell for cell in cells), "Unsupported Markdown table content")
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines)


def rendered(profile, text):
    before, content = text.split(BEGIN)
    _, after = content.split(END)
    return before + BEGIN + "\n\n" + table(profile) + "\n\n" + END + after


def check_matrix(profile, text):
    need(text == rendered(profile, text), "Stale generated API matrix")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "render"))
    args = parser.parse_args()
    try:
        profile = json.loads((PLAN / "api-profile.json").read_text(encoding="utf-8"))
        ledger = json.loads((PLAN / "tasks.json").read_text(encoding="utf-8"))
        validate(profile, ledger)
        path = PLAN / "api-compatibility.md"
        text = path.read_text(encoding="utf-8")
        if args.command == "render":
            path.write_text(rendered(profile, text), encoding="utf-8", newline="\n")
        else:
            check_matrix(profile, text)
        print("PASS: API profile provenance, alternatives, finite budgets, proposed gates, dependency ordering and matrix freshness")
        return 0
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
