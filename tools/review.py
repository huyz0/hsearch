#!/usr/bin/env python3
"""Durable independent-session review protocol for the trusted integrator."""
from contextlib import contextmanager
import argparse
import hashlib
import json
from pathlib import Path
import re
import runpy
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TASK = runpy.run_path(str(ROOT / "tools/task.py"))
need, Refused, encoded = TASK["need"], TASK["Refused"], TASK["encoded"]
SANDBOX = TASK["SANDBOX"]
HARD_CEILING, PLANNED_ROUNDS = 3, 2
ROLES = {"correctness_tests_claims", "architecture_security"}


def schema(name, value):
    TASK["validate_schema"](value, json.loads((ROOT / "harness" / ("review-" + name + ".schema.json")).read_text(encoding="utf-8")), "review")
    return value


def routes(paths):
    # Unknown/non-documentation changes fail toward the additional risk review.
    sensitive = ("architecture", "security", "api-", "replication", "engineering-standards", "ci-testing")
    ordinary_docs = all(path.startswith("docs/") and not any(word in path for word in sensitive) for path in paths)
    return ["correctness_tests_claims"] + ([] if ordinary_docs else ["architecture_security"])


def validate_history(data):
    for task, rounds in data["tasks"].items():
        need(type(rounds) is list and len(rounds) <= HARD_CEILING, "Review history/counter is invalid")
        previous = []
        for number, current in enumerate(rounds, 1):
            packet = schema("packet", current["packet"])
            need(packet["task_id"] == task and packet["round"] == number and packet["previous_findings"] == previous,
                 "Review task/round/history changed")
            need(hashlib.sha256(encoded(packet)).hexdigest() == current["sha256"] and
                 packet["packet_payload_bytes"] == len(encoded(packet)), "Review packet content changed")
            need(set(current["verdicts"]) == set(current["verdict_sha256"]) and set(current["verdicts"]).issubset(packet["required_roles"]), "Review roles changed")
            for role, value in current["verdicts"].items():
                schema("verdict", value)
                need(hashlib.sha256(encoded(value)).hexdigest() == current["verdict_sha256"][role], "Recorded review content changed")
                need(value["packet_sha256"] == current["sha256"] and value["role"] == role and
                     value["reviewer_session"] == packet["reviewers"][role] and
                     all(value[key] == packet[key] for key in ("head", "tree", "source_digest")), "Recorded review binding changed")
            previous += [{"review_key": f"{number}:{role}:{finding['id']}", **finding}
                         for role, value in current["verdicts"].items() for finding in value["findings"]]


class ReviewStore:
    def __init__(self, state_root, root=ROOT):
        self.root = Path(root).resolve()
        self.tasks = TASK["TaskStore"](Path(state_root) / "repositories", self.root)
        self.path = self.tasks.root / "reviews.json"

    @contextmanager
    def edit(self):
        with SANDBOX["file_lock"](self.tasks.root / "reviews.lock"):
            need(not self.path.is_symlink() and (not self.path.exists() or self.path.stat().st_size <= 32 * 1024**2), "Review registry redirected/over bound")
            data = json.loads(self.path.read_text(encoding="utf-8")) if self.path.exists() else {
                "schema_version": 1, "repository": self.tasks.repository, "tasks": {}}
            need(data["schema_version"] == 1 and data["repository"] == self.tasks.repository, "Review registry belongs to another repository")
            validate_history(data)
            yield data
            validate_history(data)
            need(len(data["tasks"]) <= 1024 and len(json.dumps(data, indent=2).encode()) + 1 <= 32 * 1024**2, "Review registry bound exceeded")
            SANDBOX["atomic_json"](self.path, data)

    def source(self):
        head = TASK["git"]("rev-parse", "HEAD", root=self.root).strip()
        branch = TASK["git"]("symbolic-ref", "--short", "HEAD", root=self.root).strip()
        record = {"worktree": str(self.root), "branch": branch, "packet": {"base_revision": head, "owned_paths": []}}
        identity = self.tasks.fingerprint(record)
        need(identity["clean"], "Review requires the actual committed clean source, not hidden/filtered edits")
        return {"head": head, "tree": TASK["git"]("rev-parse", "HEAD^{tree}", root=self.root).strip(), "source_digest": identity["digest"]}

    def begin(self, task, base, implementer, reviewers, checks, reason):
        need(re.fullmatch(r"M[0-9]{2}\.[0-9]{2}", task) and TASK["SHA"].fullmatch(base), "Exact task/base identity required")
        ledger = TASK["ledger_tasks"](self.root)
        need(task in ledger, "Unknown review task")
        source = self.source()
        TASK["git"]("merge-base", "--is-ancestor", base, source["head"], root=self.root)
        paths = list(filter(None, TASK["git"]("diff", "--name-only", "-z", base, source["head"], root=self.root).split("\0")))
        need(paths, "Review requires a concrete change")
        paths = [TASK["relative_path"](path) for path in paths]
        required = routes(paths)
        need(all(type(value) is str and re.fullmatch(r"[A-Za-z0-9/][A-Za-z0-9._:/-]{0,127}", value)
                 for value in [implementer, *reviewers.values()]), "Exact reviewer/implementer session identifiers required")
        need(set(reviewers) == set(required) and len(set(reviewers.values())) == len(reviewers) and implementer not in reviewers.values(), "Required reviewers must be separate independent sessions")
        need(checks and len({item["name"] for item in checks}) == len(checks) and
             all(item["head"] == source["head"] and item["conclusion"] == "success" for item in checks), "Deterministic evidence must pass at the exact reviewed head")
        with self.edit() as data:
            rounds = data["tasks"].setdefault(task, [])
            need(len(rounds) < HARD_CEILING, "Review ceiling reached: split/redesign; no further round or pass by budget")
            number = len(rounds) + 1
            value = {"schema_version": 1, "task_id": task, "round": number,
                "planned_rounds": PLANNED_ROUNDS, "hard_ceiling": HARD_CEILING, "base": base, **source,
                "implementer_session": implementer, "reviewers": reviewers, "required_roles": required,
                "changed_paths": paths, "checks": checks, "required_check_names": sorted({"ci-ok", *(item["name"] for item in checks)}), "reason": reason,
                "previous_findings": [{"review_key": f"{previous['packet']['round']}:{role}:{finding['id']}", **finding}
                                      for previous in rounds for role, verdict in previous["verdicts"].items() for finding in verdict["findings"]],
                "packet_payload_bytes": 0, "estimated_tokens_bytes_div4": 0, "token_count_is_estimate": True}
            for _ in range(6):
                size = len(encoded(value))
                value.update(packet_payload_bytes=size, estimated_tokens_bytes_div4=(size + 3) // 4)
            schema("packet", value)
            need(len(encoded(value)) == value["packet_payload_bytes"] <= 48000, "Review packet exceeds context allowance; split/reduce evidence")
            need(self.source() == source, "Source changed while preparing review packet")
            identity = hashlib.sha256(encoded(value)).hexdigest()
            rounds.append({"packet": value, "sha256": identity, "verdicts": {}, "verdict_sha256": {}})
        return {"packet_sha256": identity, "packet": value}

    def record(self, value):
        value = schema("verdict", value)
        need(value["conclusion"] == ("blocked" if any(item["priority"] != "P3" for item in value["findings"]) else "clear"), "Verdict cannot suppress blocking findings")
        for item in value["findings"]:
            TASK["relative_path"](item["file"])
        need(len({item["id"] for item in value["findings"]}) == len(value["findings"]), "Finding IDs must be unique within a review")
        with self.edit() as data:
            matches = [item for rounds in data["tasks"].values() for item in rounds if item["sha256"] == value["packet_sha256"]]
            need(len(matches) == 1, "Unknown review packet")
            current = matches[0]
            packet = current["packet"]
            need(value["role"] in packet["required_roles"] and packet["reviewers"][value["role"]] == value["reviewer_session"], "Wrong independent reviewer/session")
            need(all(value[key] == packet[key] for key in ("head", "tree", "source_digest")), "Verdict does not bind the reviewed source")
            need(self.source() == {key: packet[key] for key in ("head", "tree", "source_digest")}, "Reviewed source changed; issue a new bounded round")
            need(value["role"] not in current["verdicts"], "Recorded review is immutable; fixes need a new round")
            previous = {item["review_key"] for item in packet["previous_findings"] if item["priority"] != "P3"}
            resolved = [item["review_key"] for item in value["resolutions"]]
            need(set(resolved) == previous and len(resolved) == len(previous), "Review must explicitly verify every prior blocking finding")
            current["verdicts"][value["role"]] = value
            current["verdict_sha256"][value["role"]] = hashlib.sha256(encoded(value)).hexdigest()
        return {"recorded": value["role"], "conclusion": value["conclusion"]}

    def gate(self, task, checks):
        with self.edit() as data:
            need(task in data["tasks"] and data["tasks"][task], "No independent review packet")
            current = data["tasks"][task][-1]
            packet, verdicts = current["packet"], current["verdicts"]
            need(hashlib.sha256(encoded(packet)).hexdigest() == current["sha256"], "Review packet content changed")
            need(self.source() == {key: packet[key] for key in ("head", "tree", "source_digest")}, "Review is stale for current source")
            need(set(verdicts) == set(packet["required_roles"]), "Required independent review is missing/pending")
            need(all(hashlib.sha256(encoded(value)).hexdigest() == current["verdict_sha256"][role] for role, value in verdicts.items()), "Recorded review content changed")
            need(all(value["conclusion"] == "clear" and not any(item["priority"] != "P3" for item in value["findings"]) for value in verdicts.values()), "Blocking findings prevent integration")
            TASK["validate_schema"](checks, json.loads((ROOT / "harness/review-packet.schema.json").read_text(encoding="utf-8"))["properties"]["checks"], "checks")
            expected = set(packet["required_check_names"])
            need({item["name"] for item in checks} == expected and len(checks) == len(expected) and
                 all(item["head"] == packet["head"] and item["conclusion"] == "success" for item in checks), "Required deterministic/hosted check evidence changed, missing or stale")
            return {"gate": "pass", "task_id": task, "head": packet["head"], "tree": packet["tree"],
                    "packet_sha256": current["sha256"], "round": packet["round"], "formal_github_review": False,
                    "scope": "Trusted coordinator evidence protocol; not reviewer authentication or automatic permission to merge"}


def read_json(path):
    need(path.stat().st_size <= 131072 and not path.is_symlink(), "Review input file redirected/over bound")
    return json.loads(path.read_text(encoding="utf-8"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-root", type=Path, default=Path(tempfile.gettempdir()) / "hsearch-review")
    sub = parser.add_subparsers(dest="action", required=True)
    begin = sub.add_parser("begin")
    begin.add_argument("task")
    begin.add_argument("--base", required=True)
    begin.add_argument("--implementer", required=True)
    begin.add_argument("--reviewers", type=Path, required=True, help="JSON role to actual independent session mapping")
    begin.add_argument("--checks", type=Path, required=True, help="Trusted coordinator's observed check snapshots")
    begin.add_argument("--reason", required=True)
    record = sub.add_parser("record")
    record.add_argument("verdict", type=Path, help="Structured JSON from the assigned independent session")
    gate = sub.add_parser("gate")
    gate.add_argument("task")
    gate.add_argument("--checks", type=Path, required=True)
    args = parser.parse_args()
    try:
        store = ReviewStore(args.state_root)
        if args.action == "begin":
            result = store.begin(args.task, args.base, args.implementer, read_json(args.reviewers), read_json(args.checks), args.reason)
        elif args.action == "record":
            result = store.record(read_json(args.verdict))
        else:
            result = store.gate(args.task, read_json(args.checks))
        print(encoded(result).decode())
        return 0
    except (Refused, OSError, ValueError, KeyError, TypeError) as error:
        print(f"REFUSED: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
