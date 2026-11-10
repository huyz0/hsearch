#!/usr/bin/env python3
"""Render/check the hsearch delivery ledger. Python standard library only.

This checks evidence references and state consistency, not the truth of a product
test result. Reviewers must inspect the cited evidence before approving delivery.
"""
from __future__ import annotations

import argparse
import copy
import json
import re
import sys
from collections import Counter
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
STATES = {"planned", "in_progress", "review", "blocked", "done"}


class InvalidPlan(ValueError):
    pass


def need(condition: bool, message: str) -> None:
    if not condition:
        raise InvalidPlan(message)


def within_root(path: str) -> Path:
    resolved = (ROOT / path).resolve()
    need(resolved.is_relative_to(ROOT), f"Path escapes docs/plan: {path}")
    return resolved


def indexes(data: dict) -> tuple[dict, dict]:
    milestones, tasks = {}, {}
    for milestone in data["milestones"]:
        need(milestone["id"] not in milestones, "Duplicate milestone ID")
        milestones[milestone["id"]] = milestone
        for task in milestone["tasks"]:
            need(task["id"] not in tasks, "Duplicate task ID")
            tasks[task["id"]] = task
    return milestones, tasks


def task_dependencies(task: dict, milestones: dict, tasks: dict) -> list[str]:
    result = []
    for ref in task["depends_on"]:
        if ref in milestones:
            result.extend(t["id"] for t in milestones[ref]["tasks"])
        else:
            need(ref in tasks, f"Unknown dependency {ref} in {task['id']}")
            result.append(ref)
    return result


def research_gate_contract(milestones: dict, tasks: dict) -> None:
    """Retain research boundaries and the explicit prospective trait-start profile."""
    required = {
        "M01.08": {"M01.04", "M01.05", "M01.06", "M01.07", "M19", "M01.09"},
        "M02.02": {"M19", "M02.09"},
        "M02.04": {"M01", "M02.10"},
        "M03.02": {"M03.01"},
        "M03.03": {"M03.01", "M19", "M02.08", "M02.05"},
        "M03.04": {"M03.03"},
        "M03.07": {"M02.07", "M03.01"},
        "M03.08": {"M02.07", "M03.04", "M03.02", "M03.07"},
        "M03.05": {"M03.02", "M03.04", "M03.08"},
        "M03.06": {"M03.05", "M03.07"},
        "M07.02": {"M05", "M01"},
        "M16.04": {"M01", "M08", "M16.02"},
        "M02.03": {"M02.02"},
        "M02.05": {"M02.01", "M02.02", "M02.04", "M02.07", "M02.08", "M08.01"},
        "M02.06": {"M02.03", "M02.05", "M02.07"},
        "M02.07": {"M02.01", "M02.04"},
        "M01.14": {"M19", "M01.13", "M08.01"},
    }
    # This prospective start exception does not accept the harness, provider or
    # milestone. The historical M14 guard remains authoritative elsewhere.
    trait_start = {"M19", "M01.01", "M01.02", "M01.03",
                   "M01.05", "M01.06", "M01.07"}
    need("M03.01" in tasks and
         set(tasks["M03.01"]["depends_on"]) == trait_start and
         len(tasks["M03.01"]["depends_on"]) == len(trait_start),
         "Product start profile changed: M03.01")
    prerequisites = {"M19", "M01.02", "M01.03", "M01.04", "M01.05",
                     "M01.06", "M01.07", "M01.13", "M01.14",
                     "M02.01", "M02.08", "M08.01"}
    for task_id in ("M02.09", "M02.10"):
        need(task_id in tasks, f"Missing coordinator research task: {task_id}")
        need(tasks[task_id].get("execution_scope") == "coordinator-research",
             f"Research execution scope changed: {task_id}")
        required[task_id] = prerequisites
    for task_id, dependencies in required.items():
        need(task_id in tasks and dependencies <= set(tasks[task_id]["depends_on"]),
             f"Research acceptance gate missing: {task_id}")
    need(set(tasks["M02.02"]["depends_on"]) == {"M19", "M02.09"} and
         len(tasks["M02.02"]["depends_on"]) == 2,
         "Native engine start profile changed: M02.02")
    need("M01" in milestones["M02"]["depends_on"],
         "Research acceptance gate missing: milestone M02")
    need(set(tasks["M03.02"]["depends_on"]) == {"M03.01"} and
         len(tasks["M03.02"]["depends_on"]) == 1,
         "Provider start profile changed: M03.02")
    need("M01" in milestones["M03"]["depends_on"],
         "Product acceptance gate missing: milestone M03")


def incident_gate_contract(tasks: dict) -> None:
    """An unresolved incident needs its own accepted replacement policy."""
    need("M01.15" in tasks, "Missing incident disposition task")
    successor = tasks["M01.15"]
    need({"M19", "M01.04", "M01.05", "M01.06", "M01.07", "M01.13"}
         <= set(successor["depends_on"]), "Incident disposition prerequisite missing")
    incident = tasks["M01.09"]
    need(incident.get("acceptance_route") == "M01.15-unresolved-incident" and
         incident.get("incident_cause_status") == "unconfirmed" and
         incident.get("incident_run") == "41283307456",
         "Incident disposition must retain the unresolved original cause")
    if incident["status"] == "done":
        need(successor["status"] == "done", "Incident disposition is not accepted")
        need(any(item.get("path") == "evidence/m01-15-incident-disposition.md"
                 for item in incident["evidence"]) and
             any(item.get("path") == "evidence/m01-15-incident-disposition.md"
                 for item in successor["evidence"]),
             "Incident disposition acceptance record missing")


def validate(data: dict, files: bool = True) -> None:
    need(isinstance(data, dict), "Ledger must be an object")
    need(data.get("schema_version") == 1, "Unsupported ledger schema")
    need(isinstance(data.get("milestones"), list) and bool(data["milestones"]), "No milestones")
    for milestone in data["milestones"]:
        need(isinstance(milestone, dict) and isinstance(milestone.get("tasks"), list), "Invalid milestone structure")
        need(isinstance(milestone.get("id"), str), "Missing milestone ID")
        for task in milestone["tasks"]:
            need(isinstance(task, dict) and isinstance(task.get("id"), str), "Invalid task structure")
    milestones, tasks = indexes(data)
    for mid, milestone in milestones.items():
        need(re.fullmatch(r"M\d{2}", mid) is not None, f"Invalid milestone ID: {mid}")
        for key in ("title", "goal", "definition_of_done", "owner", "document"):
            need(bool(milestone.get(key)), f"Missing milestone {key}: {mid}")
        for key in ("title", "goal", "owner", "document"):
            need(isinstance(milestone[key], str), f"Invalid milestone {key}: {mid}")
        for key in ("definition_of_done", "depends_on"):
            need(isinstance(milestone.get(key), list) and all(isinstance(x, str) and x for x in milestone[key]),
                 f"Invalid milestone {key}: {mid}")
        need(milestone.get("status") in STATES, f"Invalid milestone status: {mid}")
        need(bool(milestone["tasks"]), f"Empty milestone: {mid}")
        within_root(milestone["document"])
        for dep in milestone["depends_on"]:
            need(dep in milestones and dep != mid, f"Invalid milestone prerequisite: {mid}")
        all_done = all(t["status"] == "done" for t in milestone["tasks"])
        need((milestone["status"] == "done") == all_done,
             f"Milestone/task completion mismatch: {mid}")
        if milestone["status"] == "done":
            need(all(milestones[d]["status"] == "done" for d in milestone["depends_on"]),
                 f"Done milestone has unfinished prerequisite: {mid}")
        for task in milestone["tasks"]:
            tid = task["id"]
            need(re.fullmatch(re.escape(mid) + r"\.\d{2}", tid) is not None,
                 f"Task ID does not belong to milestone: {tid}")
            for key in ("title", "goal", "deliverables", "validation", "definition_of_done", "owner"):
                need(bool(task.get(key)), f"Missing task {key}: {tid}")
            for key in ("title", "goal", "owner"):
                need(isinstance(task[key], str), f"Invalid task {key}: {tid}")
            for key in ("deliverables", "validation", "definition_of_done", "depends_on"):
                need(isinstance(task.get(key), list) and all(isinstance(x, str) and x for x in task[key]),
                     f"Invalid task {key}: {tid}")
            need(task.get("status") in STATES, f"Invalid task status: {tid}")
            need(isinstance(task.get("depends_on"), list), f"Missing dependencies: {tid}")
            need(isinstance(task.get("evidence"), list), f"Missing evidence list: {tid}")
            if task["status"] == "blocked":
                need(bool(task.get("blocker")) and bool(task.get("next_action")),
                     f"Blocked task needs reason and next action: {tid}")
            if task["status"] == "done":
                need(bool(task["evidence"]), f"Done task lacks evidence: {tid}")
                for evidence in task["evidence"]:
                    need(isinstance(evidence, dict) and isinstance(evidence.get("path"), str)
                         and bool(evidence["path"]) and isinstance(evidence.get("result"), str)
                         and bool(evidence["result"]),
                         f"Incomplete evidence record: {tid}")
                    path = within_root(evidence["path"])
                    if files:
                        need(path.is_file(), f"Missing evidence file: {tid}: {path}")
    edges = {tid: task_dependencies(t, milestones, tasks) for tid, t in tasks.items()}
    milestone_visiting, milestone_visited = set(), set()

    def visit_milestone(mid: str) -> None:
        need(mid not in milestone_visiting, f"Milestone prerequisite cycle at {mid}")
        if mid in milestone_visited:
            return
        milestone_visiting.add(mid)
        for dep in milestones[mid]["depends_on"]:
            visit_milestone(dep)
        milestone_visiting.remove(mid)
        milestone_visited.add(mid)

    for mid in milestones:
        visit_milestone(mid)
    visiting, visited = set(), set()

    def visit(tid: str) -> None:
        need(tid not in visiting, f"Dependency cycle at {tid}")
        if tid in visited:
            return
        visiting.add(tid)
        for dep in edges[tid]:
            visit(dep)
        visiting.remove(tid)
        visited.add(tid)

    for tid, task in tasks.items():
        visit(tid)
        if task["status"] in {"in_progress", "review", "done"}:
            need(all(tasks[d]["status"] == "done" for d in edges[tid]),
                 f"Active/done task has unfinished dependency: {tid}")
    research_gate_contract(milestones, tasks)
    incident_gate_contract(tasks)


def bullet(items: list[str]) -> str:
    return "\n".join(f"- {item}" for item in items)


def milestone_text(m: dict) -> str:
    lines = [f"# {m['id']}: {m['title']}", "",
             "Generated from `tasks.json`; edit the ledger and run `python docs/plan/tools/plan.py render`.", "",
             f"Status: **{m['status']}**. Owner role: {m['owner']}.", "",
             "## Goal", "", m["goal"], "",
             "## Completion prerequisites", "",
             ", ".join(m["depends_on"]) or "None.", "",
             "Task-level dependencies determine when individual tasks can start. Milestone prerequisites gate completion.", "",
             "## Definition of done", "", bullet(m["definition_of_done"]), "",
             "## Tasks", ""]
    for t in m["tasks"]:
        lines += [f"### {t['id']}: {t['title']}", "",
                  f"Status: **{t['status']}**. Owner role: {t['owner']}.", "",
                  f"Goal: {t['goal']}", "",
                  f"Depends on: {', '.join(t['depends_on']) or 'None'}.", "",
                  "Deliverables:", "", bullet(t["deliverables"]), "",
                  "Validation:", "", bullet(t["validation"]), "",
                  "Definition of done:", "", bullet(t["definition_of_done"]), ""]
        if t.get("blocker"):
            lines += [f"Blocker: {t['blocker']}", "", f"Next action: {t['next_action']}", ""]
        if t["evidence"]:
            lines += ["Evidence:", ""]
            for e in t["evidence"]:
                lines += [f"- [{e['path']}](../{e['path']}): {e['result']}"]
            lines += [""]
    return "\n".join(lines)


def generated(data: dict) -> dict[Path, str]:
    result = {within_root(m["document"]): milestone_text(m) for m in data["milestones"]}
    lines = ["# Delivery roadmap", "",
             "Generated from `tasks.json`. All product capabilities are planned until cited evidence closes their tasks.", "",
             "## Research first", "",
             "Cost research and architecture decisions belong to M00.07, before runtime work. See [completed decisions](research/cost-decisions.md) and [executed experiments](evidence/cost-research-results.md). Implementation cost gates are inside M02/M03/M06/M07/M09; real-provider reconciliation remains in M15. There is no active M18.", "",
             "Task dependencies permit parallel work; milestone prerequisites govern completion. Owners are roles to allocate, not people already assigned.", "",
             "| Milestone | Status | Tasks done | Completion prerequisites |", "| --- | --- | --- | --- |"]
    for m in data["milestones"]:
        count = sum(t["status"] == "done" for t in m["tasks"])
        lines += [f"| [{m['id']}: {m['title']}]({m['document']}) | {m['status']} | {count}/{len(m['tasks'])} | {', '.join(m['depends_on']) or 'None'} |"]
    lines += ["", "## Operating contract", "",
              "A task is done only when its own definition of done, milestone release boundaries, applicable coverage/security gates, and final review are satisfied. Cite immutable run/commit artifacts through a local evidence record. A file's existence alone does not prove the result it describes.", "",
              "Use `python docs/plan/tools/plan.py next` to find dependency-ready planned tasks. Reserve path/resource ownership before allocation. See [AI harness](ai-harness.md), [CI/testing](ci-testing.md), [requirements](requirements.md), and [progress](PROGRESS.md).", ""]
    result[ROOT / "roadmap.md"] = "\n".join(lines)
    return result


def stale_outputs(data: dict) -> list[str]:
    return [str(path.relative_to(ROOT)) for path, expected in generated(data).items()
            if not path.exists() or path.read_text(encoding="utf-8") != expected]


def heading_anchors(text: str) -> set[str]:
    anchors, counts = set(), Counter()
    for line in text.splitlines():
        if not re.match(r"^#{1,6} ", line):
            continue
        label = re.sub(r"^#{1,6} ", "", line).strip().lower()
        label = re.sub(r"[^\w\- ]", "", label).replace(" ", "-")
        n = counts[label]
        counts[label] += 1
        anchors.add(label if not n else f"{label}-{n}")
    return anchors


def links() -> int:
    checked = 0
    pattern = re.compile(r"\[[^\]\n]*\]\(([^)\n]+)\)")
    for source in ROOT.rglob("*.md"):
        text = source.read_text(encoding="utf-8")
        for raw in pattern.findall(text):
            target = raw.strip().strip("<>")
            parsed = urlsplit(target)
            if parsed.scheme or target.startswith("//"):
                continue
            path = (source.parent / unquote(parsed.path)).resolve() if parsed.path else source
            need(path.is_relative_to(ROOT), f"Link escapes plan: {source.name}: {target}")
            need(path.exists(), f"Broken link: {source.relative_to(ROOT)}: {target}")
            if parsed.fragment:
                need(path.is_file() and unquote(parsed.fragment) in heading_anchors(path.read_text(encoding="utf-8")),
                     f"Broken anchor: {source.relative_to(ROOT)}: {target}")
            checked += 1
    return checked


def selftest(data: dict) -> int:
    cases = []
    def case(name, expected, mutate):
        candidate = copy.deepcopy(data)
        mutate(candidate)
        try:
            validate(candidate, files=False)
        except InvalidPlan as error:
            need(expected in str(error), f"Control failed for wrong reason: {name}: {error}")
            cases.append(name)
            return
        raise InvalidPlan(f"Negative control unexpectedly passed: {name}")

    def task(d, tid):
        return indexes(d)[1][tid]

    def milestone(d, mid):
        return indexes(d)[0][mid]

    case("unknown dependency", "Unknown dependency", lambda d: task(d, "M00.01")["depends_on"].append("M99.99"))
    case("self cycle", "Dependency cycle", lambda d: task(d, "M00.01")["depends_on"].append("M00.01"))
    case("duplicate task", "Duplicate task", lambda d: milestone(d, "M00")["tasks"].append(copy.deepcopy(task(d, "M00.01"))))
    case("missing done evidence", "Done task lacks evidence", lambda d: task(d, "M00.01").update(status="done", evidence=[]))
    case("false milestone completion", "Milestone/task completion mismatch", lambda d: milestone(d, "M01").update(status="done"))
    case("invalid status", "Invalid task status", lambda d: task(d, "M01.01").update(status="passed"))
    case("missing task acceptance", "Missing task definition_of_done", lambda d: task(d, "M00.01").update(definition_of_done=[]))
    case("unexplained blocker", "Blocked task needs reason", lambda d: task(d, "M01.01").update(status="blocked", blocker="", next_action=""))
    case("path traversal", "Path escapes", lambda d: milestone(d, "M00").update(document="../escape.md"))
    case("wrong field type", "Invalid task validation", lambda d: task(d, "M00.01").update(validation="not a list"))
    case("unknown schema", "Unsupported ledger schema", lambda d: d.update(schema_version=99))
    case("cross-task cycle", "Dependency cycle", lambda d: task(d, "M00.01")["depends_on"].append("M00.02"))
    def unfinished(d):
        # Bind by stable ID: roadmap priority may change independently of IDs.
        task(d, "M01.01").update(depends_on=[], status="planned")
        task(d, "M00.01")["depends_on"].append("M01.01")
    case("unfinished dependency", "Active/done task has unfinished dependency", unfinished)
    case("milestone completion cycle", "Milestone prerequisite cycle", lambda d: milestone(d, "M01").update(depends_on=["M02"]))
    case("evidence traversal", "Path escapes", lambda d: task(d, "M00.01")["evidence"][0].update(path="../outside.txt"))
    candidate = copy.deepcopy(data)
    milestone(candidate, "M00")["title"] += " stale control"
    need(bool(stale_outputs(candidate)), "Stale-render control unexpectedly passed")
    cases.append("stale generated artifact")
    print(f"PASS: {len(cases)} negative controls: {', '.join(cases)}")
    return len(cases)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "render", "status", "next", "selftest"))
    args = parser.parse_args()
    try:
        data = json.loads((ROOT / "tasks.json").read_text(encoding="utf-8"))
        validate(data)
        milestones, tasks = indexes(data)
        if args.command == "render":
            for path, content in generated(data).items():
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8", newline="\n")
            print(f"Rendered {len(milestones)} milestones and roadmap")
        elif args.command == "check":
            stale = stale_outputs(data)
            need(not stale, f"Stale generated outputs: {', '.join(stale)}")
            count = links()
            print(f"PASS: {len(milestones)} milestones, {len(tasks)} tasks, dependency DAG, completion/evidence rules, {count} local links, generated output freshness")
        elif args.command == "selftest":
            selftest(data)
        elif args.command == "status":
            print(json.dumps({"milestones": dict(Counter(m["status"] for m in milestones.values())),
                              "tasks": dict(Counter(t["status"] for t in tasks.values()))}, indent=2))
        elif args.command == "next":
            for task in tasks.values():
                deps = task_dependencies(task, milestones, tasks)
                if task["status"] == "planned" and all(tasks[d]["status"] == "done" for d in deps):
                    print(f"{task['id']} {task['title']} [{task['owner']}]")
        return 0
    except (InvalidPlan, KeyError, TypeError, OSError, json.JSONDecodeError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
