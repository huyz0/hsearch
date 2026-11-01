#!/usr/bin/env python3
"""Qualify real pinned Rust navigation, isolation, freshness and whole fixture costs."""
import argparse
import json
from pathlib import Path
import runpy
import os
import stat
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
API = runpy.run_path(str(ROOT / "tools/context.py"))
need, encoded = API["need"], API["encoded"]


def cleanup_claims(store, claims, resources, query, known_edits, primary_error):
    errors = []
    for claim, role in reversed(claims):
        try:
            if store.get(claim)["phase"] == "interrupted":
                store.resume(claim, role, resources)
            stopped = query(claim, role, "stop")
            need(stopped["status"] == "stopped", "Owned semantic cleanup uncertain; inspect retained evidence")
            for path, before, after in known_edits.get(claim, []):
                worktree = Path(store.get(claim)["worktree"])
                need(path.is_relative_to(worktree) and path.resolve() == path.absolute(), "Redirected fixture edit preserved")
                info = path.lstat()
                need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= max(len(before), len(after)),
                     "Unexpected fixture type/size/link preserved")
                with path.open("rb") as source:
                    actual = source.read(max(len(before), len(after)) + 1)
                need(actual in (before, after), "Unexpected fixture edit preserved for inspection")
                if actual == after:
                    descriptor, temporary = tempfile.mkstemp(prefix=".hsearch-restore-", dir=path.parent)
                    try:
                        with os.fdopen(descriptor, "wb") as restored:
                            restored.write(before)
                            restored.flush()
                            os.fsync(restored.fileno())
                        os.chmod(temporary, info.st_mode & 0o777)
                        os.replace(temporary, path)
                    finally:
                        Path(temporary).unlink(missing_ok=True)
            store.cleanup(claim, role, resources)
        except Exception as error:
            errors.append({"claim": claim, "cleanup_error": str(error)[:256]})
    for error in errors:
        print(json.dumps(error), flush=True)
    if errors and primary_error is None:
        raise API["Refused"]("Owned corpus cleanup incomplete; inspect the recorded errors")


def corpus(resources, evidence_parent):
    evidence_parent = evidence_parent.resolve()
    evidence_parent.mkdir(parents=True, exist_ok=True)
    area = Path(tempfile.mkdtemp(prefix="navigation-corpus-", dir=evidence_parent))
    source = area / "source"
    source.mkdir()
    files = {
        "Cargo.toml": '[package]\nname="navigation_fixture"\nversion="0.0.0"\nedition="2021"\n',
        "Cargo.lock": 'version = 4\n\n[[package]]\nname = "navigation_fixture"\nversion = "0.0.0"\n',
        "src/lib.rs": 'pub mod model;\npub fn greet(user: &model::User) -> u32 { user.id() }\n',
        "src/model.rs": 'pub struct User { value: u32 }\nimpl User {\n    pub fn new(value: u32) -> Self { Self { value } }\n    pub fn id(&self) -> u32 { self.value }\n}\n',
        "review/task_reference.py": (ROOT / "tools/task.py").read_text(encoding="utf-8"),
        "docs/plan/tasks.json": json.dumps({"milestones": [{"id": "M01", "status": "in_progress", "tasks": [
            {"id": task, "status": "in_progress", "depends_on": []} for task in ("M01.04", "M01.05")]}]}),
    }
    for name, value in files.items():
        path = source / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value, encoding="utf-8", newline="\n")

    def git(root, *args):
        return API["TASK"]["git"](*args, root=root).strip()

    git(source, "init", "-q", "-b", "main")
    git(source, "config", "user.name", "Navigation corpus")
    git(source, "config", "user.email", "navigation@example.invalid")
    git(source, "config", "core.autocrlf", "false")
    git(source, "add", ".")
    git(source, "commit", "-qm", "frozen navigation fixture")
    store = API["TASK"]["TaskStore"](area / "tasks", source)
    claims, events, results, known_edits = [], [], [], {}

    def query(claim, owner, operation, **fields):
        value = {"schema_version": 1, "operation": operation, **fields}
        started = time.monotonic()
        result = API["route"](store, claim, owner, value, resources)
        event = {"role": owner, "request": value, "response": result}
        events.append(event)
        results.append({"operation": operation, "owner": owner, "status": result["status"],
                        "elapsed_seconds": round(time.monotonic() - started, 3)})
        if result.get("reason"):
            results[-1]["reason"] = result["reason"]
        if result.get("data", {}).get("semantic_failure"):
            results[-1]["semantic_failure"] = result["data"]["semantic_failure"]
        with (area / "events.jsonl").open("a", encoding="utf-8") as log:
            log.write(json.dumps(event, ensure_ascii=False) + "\n")
        print(json.dumps(results[-1]), flush=True)
        return result

    try:
        for task, owner, paths in (("M01.04", "implementer", ["src"]), ("M01.05", "reviewer", ["review"])):
            packet = {"schema_version": 1, "task_id": task, "base_revision": git(source, "rev-parse", "HEAD"),
                      "owner_session": owner, "goal": "Qualify bounded semantic context and independent fixture costs",
                      "owned_paths": paths, "assumptions": [], "acceptance": ["Exact independent source-location oracle"],
                      "negative_cases": ["Stale server is not semantic; compile errors fail"], "commands": ["prefetch", "check"],
                      "output_budget_bytes": 1048576, "expected_evidence": ["Real bounded session observations"]}
            claims.append((store.claim(packet), owner))
        first, owner = claims[0]
        second, reviewer = claims[1]
        initial = store.fingerprint(store.get(first))
        first_root = Path(store.get(first)["worktree"])
        first_ready = query(first, owner, "start")
        need(first_ready["status"] == "ready", "First real semantic session failed")
        doctor = query(first, owner, "doctor")
        need(doctor["status"] == "doctor", "Pinned CLI doctor failed")
        manager = doctor["data"]["manager_pid"]
        outline = query(first, owner, "outline", file="src/model.rs")
        need(outline["status"] == "semantic" and {item["name"] for item in outline["data"]["items"]} == {"User", "impl User"}, "Outline oracle mismatch")
        definition = query(first, owner, "definition", file="src/lib.rs", line=2, find="pub fn greet(user: &model::<|>User)")
        locations = definition.get("data", {}).get("locations", [])
        need(definition["status"] == "semantic" and [(x["uri"], x["line"], x["character"]) for x in locations] == [("/workspace/src/model.rs", 1, 11)], "Definition location oracle mismatch")
        references = query(first, owner, "reference", file="src/model.rs", line=4, find="pub fn <|>id", max_items=1)
        locations = references.get("data", {}).get("locations", [])
        need(references["status"] == "semantic" and [(x["uri"], x["line"], x["character"]) for x in locations] == [("/workspace/src/lib.rs", 2, 47)], "Reference location oracle mismatch")
        rename = query(first, owner, "rename-preview", file="src/model.rs", line=4, find="pub fn <|>id", new_name="number")
        need(rename["status"] == "semantic" and rename["data"]["applied"] is False and rename["data"]["editCount"] == 2 and
             {x["uri"] for x in rename["data"]["files"]} == {"/workspace/src/model.rs", "/workspace/src/lib.rs"}, "Rename preview oracle mismatch")
        need(store.fingerprint(store.get(first))["digest"] == initial["digest"], "Semantic preview modified source")
        diagnostics = query(first, owner, "diagnostics", file="src/lib.rs")
        need(diagnostics["status"] == "semantic" and diagnostics["data"]["items"] == [] and diagnostics["cargo_check_required"], "Valid fixture diagnostic oracle mismatch")
        warm = query(first, owner, "doctor")
        need(warm["data"]["manager_pid"] == manager, "Warm request restarted the daemon")
        second_ready = query(second, reviewer, "start")
        need(second_ready["status"] == "ready" and second_ready["data"]["namespace"] != first_ready["data"]["namespace"], "Worktrees did not receive distinct live namespaces")
        module = runpy.run_path(str(ROOT / "tools/navigation.py"))
        nav = module["Navigation"](store, first, owner, resources, initial)
        nav.connect()
        a, b = nav.registry.get(first_ready["data"]["namespace"]), nav.registry.get(second_ready["data"]["namespace"])
        need(a["container_id"] != b["container_id"] and a["harness"] != b["harness"] and a["source"] != b["source"], "Semantic process/source/snapshot isolation failed")
        need(nav.inspect(a, a["container_id"])["State"]["Running"] and nav.inspect(b, b["container_id"])["State"]["Running"], "Concurrent daemon oracle failed")
        need(query(second, reviewer, "stop")["status"] == "stopped", "Second namespace cleanup failed")
        need(query(first, owner, "doctor")["data"]["manager_pid"] == manager, "Other worktree cleanup interrupted warm daemon")
        need(query(first, owner, "stop")["status"] == "stopped", "First workflow lifecycle did not close")

        # The full deterministic workflow includes lifecycle, semantic output,
        # compiler/review messages and both roles. Estimates are not billed tokens.
        semantic_events = list(events)
        common = [{"role": "implementer", "instruction": "Find User and id, inspect callers, preview rename and check compilation."},
                  {"role": "reviewer", "instruction": "Independently inspect both declarations and callers; report findings."}]
        baseline = [{"role": role, "file": name, "text": files[name]} for role in (owner, reviewer) for name in ("src/lib.rs", "src/model.rs")]
        narrow_start = len(events)
        reference_lines = files["review/task_reference.py"].splitlines()
        line = next(n for n, text in enumerate(reference_lines, 1) if text.startswith("    def cleanup("))
        for claim, role in claims:
            query(claim, role, "search", path="review", pattern="def cleanup(", max_items=1)
            query(claim, role, "slice", file="review/task_reference.py", start_line=line, end_line=min(line + 39, len(reference_lines)))
        narrow_events = events[narrow_start:]
        narrow_baseline = [{"role": role, "file": "review/task_reference.py", "text": files["review/task_reference.py"]} for role in (owner, reviewer)]

        # Editing our own claimed fixture makes the running server stale. Never
        # silently reuse it or represent literal matches as semantic results.
        need(query(first, owner, "start")["status"] == "ready", "Freshness fixture start failed")
        changed = files["src/lib.rs"].replace("user.id()", '"wrong type"')
        known_edits[first] = [(first_root / "src/lib.rs", files["src/lib.rs"].encode(), changed.encode())]
        (first_root / "src/lib.rs").write_text(changed, encoding="utf-8", newline="\n")
        stale = query(first, owner, "outline", file="src/lib.rs")
        need(stale["status"] == "fallback" and not stale["semantic"] and "stale_semantic_session" in stale["reason"], "Stale server did not fall back honestly")
        need(query(first, owner, "start")["status"] == "ready", "Explicit stale-session replacement failed")
        cold_diagnostic_sessions = []
        for repetition in range(5):
            if repetition:
                need(query(first, owner, "stop")["status"] == "stopped", "Cold diagnostic session did not stop")
                need(query(first, owner, "start")["status"] == "ready", "Cold diagnostic session did not start")
            invalid = query(first, owner, "diagnostics", file="src/lib.rs")
            need(invalid["cargo_check_required"] and invalid["status"] == "semantic", "Cold diagnostics must return semantic data with compiler authority declared")
            need(any(item.get("severity") == "error" and item.get("code") == "E0308" and item.get("line") == 2
                     for item in invalid["data"]["items"]), "Cold diagnostic E0308 location oracle mismatch")
            cold_diagnostic_sessions.append({"repetition": repetition, "source_digest": invalid["source"]["digest"],
                                             "expected_code": "E0308", "expected_line": 2})
        failed = query(first, owner, "compile")
        need(failed["status"] == "compile" and failed["data"]["passed"] is False, "Authoritative cargo check accepted type error")
        store.resume(first, owner, resources)
        (first_root / "src/lib.rs").write_text(files["src/lib.rs"], encoding="utf-8", newline="\n")
        passed = query(first, owner, "compile")
        need(passed["status"] == "compile" and passed["data"]["passed"] is True, "Authoritative cargo check rejected valid fixture")

        shared = common + [{"role": "compiler", "request": {"operation": "compile"}, "response": passed},
                           {"role": "reviewer", "findings": [], "basis": "Frozen location oracle and source-preservation assertions above"}]
        def costs(name, before, after):
            baseline_bytes, routed_bytes = len(encoded(shared + before)), len(encoded(shared + after))
            return {"fixture": name, "whole_baseline_payload_bytes": baseline_bytes, "whole_routed_payload_bytes": routed_bytes,
                    "baseline_estimated_tokens_bytes_div4": (baseline_bytes + 3) // 4,
                    "routed_estimated_tokens_bytes_div4": (routed_bytes + 3) // 4,
                    "routed_minus_baseline_payload_bytes": routed_bytes - baseline_bytes}
        report = {"corpus": "pass", "observed_capacity": nav.docker.capacity, "identity": first_ready["data"]["identity"],
                  "independent_location_oracle": True, "rename_writes": False, "concurrent_isolated_worktrees": True,
                  "warm_manager_reused": True, "other_worktree_survived_cleanup": True, "stale_fallback": True,
                  "cargo_check_type_error_failed": True, "cargo_check_valid_passed": True,
                  "cold_diagnostic_sessions": cold_diagnostic_sessions,
                  "accounting_scope": "Complete deterministic fixture JSON transcript including both roles, commands, responses and fixed review; excludes model internals, actual token billing, transport framing and unrelated tools",
                  "tokenizer": None, "token_counts_are_estimates": True, "actual_agent_trial": "M01.08 remains separate",
                  "costs": [costs("tiny Rust semantic workflow including cold/warm lifecycle", baseline, semantic_events),
                            costs("two-role bounded retrieval of cleanup in actual task controller source", narrow_baseline, narrow_events)],
                  "observations": results, "evidence_directory": str(area)}
    finally:
        cleanup_claims(store, claims, resources, query, known_edits, sys.exception())
    report["owned_claims_closed"] = True
    (area / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8", newline="\n")
    print(json.dumps(report, sort_keys=True), flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-root", required=True, type=Path, help="Existing shared sandbox resource registry with built image")
    parser.add_argument("--evidence-parent", required=True, type=Path, help="New owned fixtures and evidence are retained here")
    args = parser.parse_args()
    try:
        corpus(args.state_root, args.evidence_parent)
        return 0
    except (API["Refused"], OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(json.dumps({"corpus": "failed", "reason": str(error)}), flush=True)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
