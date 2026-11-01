#!/usr/bin/env python3
"""Run frozen-source native coverage through the existing owned Linux sandbox."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import runpy
import sys

ROOT = Path(__file__).resolve().parents[1]
SANDBOX = runpy.run_path(str(ROOT / "tools/sandbox.py"))
TASK = runpy.run_path(str(ROOT / "tools/task.py"))
SOURCES = runpy.run_path(str(ROOT / "tools/coverage_sources.py"))


def need(condition, message):
    if not condition:
        raise ValueError(message)


def freeze_bundle(path, *, limit=128 * 1024**2):
    """Bound binary Git output before creating any bundle file."""
    need(type(limit) is int and 0 < limit <= 128 * 1024**2 and not path.exists(), "Invalid/existing bundle output")
    objects = SOURCES["GitObjects"](ROOT, SOURCES["Budget"](SOURCES["Limits"]()))
    raw = objects.command("-c", "pack.threads=1", "-c", "pack.windowMemory=64m", "-c", "pack.deltaCacheSize=32m",
                          "bundle", "create", "-", "HEAD", limit=limit)
    need(0 < len(raw) <= limit, "Frozen source bundle exceeds bound")
    with path.open("xb") as output: output.write(raw)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-root", type=Path, required=True)
    parser.add_argument("--base", required=True)
    args = parser.parse_args()
    need(args.base == "parentless-root" or re.fullmatch(r"[0-9a-f]{40}", args.base), "Exact source base or proven root required")
    head = TASK["git"]("rev-parse", "HEAD", root=ROOT).strip()
    before = TASK["source_fingerprint"](ROOT, head, [])
    need(before["clean"], "Freeze clean committed source before local coverage")
    SOURCES["prove"](ROOT, head=head, base=None if args.base == "parentless-root" else args.base, sources={})
    sandbox = SANDBOX["Sandbox"](args.state_root)
    sandbox.image()
    namespace = None
    try:
        namespace = sandbox.up("coverage")
        sandbox.run(namespace, "prefetch")
        record = sandbox.registry.get(namespace)
        snapshot = sandbox.snapshot(record)
        bundle = snapshot / "coverage-source.bundle"
        need(not bundle.exists(), "Preserve existing source bundle")
        freeze_bundle(bundle)
        need(bundle.is_file() and 0 < bundle.stat().st_size <= 128 * 1024**2, "Frozen source bundle exceeds bound")
        need(TASK["source_fingerprint"](ROOT, head, []) == before, "Source changed while freezing bundle")
        config = {"schema_version": 2, "head": head, "base": args.base, "source_fingerprint": before["digest"],
                  "bundle_sha256": hashlib.sha256(bundle.read_bytes()).hexdigest()}
        config_path = snapshot / "coverage-source.json"
        with config_path.open("x", encoding="utf-8", newline="\n") as target:
            target.write(json.dumps(config, sort_keys=True) + "\n")
        pins = {}
        for path in (bundle, config_path):
            path.chmod(0o444)
            pins[path.name] = {"bytes": path.stat().st_size, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
        sandbox.registry.update(namespace, coverage_inputs=pins)
        output = snapshot / "coverage-evidence"
        sandbox.run(namespace, "coverage", artifact_directory=output)
        summary_path = output / "collection/summary.json"
        need(summary_path.is_file() and summary_path.stat().st_size <= 4 * 1024**2, "Missing bounded local coverage summary")
        summary = json.loads(summary_path.read_text(encoding="utf-8"))
        need(summary["passed"] is True and summary["source"]["head"] == head and
             {value["profile"] for value in summary["profiles"]} == {"dev", "release"}, "Incomplete local native coverage matrix")
        need(TASK["source_fingerprint"](ROOT, head, []) == before, "Host source changed during local coverage")
        print(json.dumps({"local_coverage": "pass", "head": head, "profiles": ["dev", "release"], "summary": str(summary_path)}), flush=True)
    finally:
        primary = sys.exception()
        if namespace:
            try:
                sandbox.down(namespace)
            except Exception as cleanup_error:
                if primary is None:
                    raise
                primary.add_note("Coverage cleanup retained uncertain resources: " + str(cleanup_error)[:512])


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError) as error:
        print("LOCAL COVERAGE REFUSED: " + str(error)[:4096], file=sys.stderr)
        raise SystemExit(1)
