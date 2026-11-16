#!/usr/bin/env python3
"""M13.01: the M02.08 catalog model re-keyed on the index, run at 100x its idle count.

M02.06 condition C8: the original model keyed rows by (tenant, collection) and proved fixed work at
1,000 vs 10,000 idle collections. With tenant = index (product-requirements, conflict 3) the key is
the index alone. This builds that catalog (one row per index, one route row per idle index, sixteen
for the hot one) at 100,000 and 1,000,000 idle indexes, and checks in fresh processes that selecting
the hot index costs the same two statements and 17 rows, uses only indexed plans, and does not grow
the process's peak RSS by more than 10%. A full-scan control must refuse. Synthetic SQLite model:
not the runtime catalog, not physical I/O, not a fleet result. Run:

    python3 tools/catalog_index_scale.py --output /private/new-directory [--small 100000 --large 1000000]
"""
from __future__ import annotations

import argparse
import hashlib
import json
import platform
import sqlite3
import statistics
import subprocess
import sys
from contextlib import closing
from pathlib import Path

HASH_SPACE = 65536
HOT = "hot-index-000000000000000000000000"
SELECT_INDEX = "SELECT incarnation,generation FROM indexes WHERE index_id=?"
SELECT_ROUTES = ("SELECT partition,lower,upper,reader FROM routes WHERE index_id=? AND incarnation=? "
                 "AND generation=? AND lower<? AND upper>? ORDER BY partition LIMIT ?")
SCAN = "SELECT index_id FROM indexes LIMIT 2000000"


def idle_id(n: int) -> str:
    return "idle-" + format(n, "030d")


def build(path: Path, idle: int) -> None:
    with closing(sqlite3.connect(path)) as db:
        db.executescript("""
            PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
            CREATE TABLE indexes (index_id TEXT NOT NULL PRIMARY KEY, incarnation INTEGER NOT NULL,
                                  generation INTEGER NOT NULL) WITHOUT ROWID;
            CREATE TABLE routes (index_id TEXT NOT NULL, incarnation INTEGER NOT NULL,
                                 generation INTEGER NOT NULL, partition INTEGER NOT NULL,
                                 lower INTEGER NOT NULL, upper INTEGER NOT NULL, reader INTEGER NOT NULL,
                                 PRIMARY KEY (index_id, incarnation, generation, partition)) WITHOUT ROWID;
        """)
        db.execute("INSERT INTO indexes VALUES (?,1,1)", (HOT,))
        db.executemany("INSERT INTO routes VALUES (?,1,1,?,?,?,1)",
                       ((HOT, p, p * HASH_SPACE // 16, (p + 1) * HASH_SPACE // 16) for p in range(16)))
        for start in range(0, idle, 5000):
            ids = [idle_id(n) for n in range(start, min(start + 5000, idle))]
            db.executemany("INSERT INTO indexes VALUES (?,1,1)", ((i,) for i in ids))
            db.executemany("INSERT INTO routes VALUES (?,1,1,0,0,?,1)", ((i, HASH_SPACE) for i in ids))
        db.commit()


def rss(field: str) -> int:
    for line in Path("/proc/self/status").read_text().splitlines():
        if line.startswith(field + ":"):
            return int(line.split()[1]) * 1024
    raise SystemExit("RSS unavailable")


def probe(path: Path, iterations: int, variant: str) -> dict:
    statements = []
    with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)) as db:
        db.execute("PRAGMA query_only=ON"); db.execute("PRAGMA mmap_size=0"); db.execute("PRAGMA cache_size=-256")
        db.set_trace_callback(lambda s: statements.append(s) if s.lstrip().upper().startswith("SELECT ") else None)
        plans = [r[3] for r in db.execute("EXPLAIN QUERY PLAN " + SELECT_INDEX, (HOT,))]
        plans += [r[3] for r in db.execute("EXPLAIN QUERY PLAN " + SELECT_ROUTES, (HOT, 1, 1, HASH_SPACE, 0, 17))]
        if variant == "full_scan":
            plans += [r[3] for r in db.execute("EXPLAIN QUERY PLAN " + SCAN)]
        statements.clear()
        rows_per_request = 0
        for _ in range(iterations):
            statements.clear()
            if variant == "full_scan":
                held = db.execute(SCAN).fetchmany(2000000)
                assert held
            inc, gen = db.execute(SELECT_INDEX, (HOT,)).fetchone()
            routes = db.execute(SELECT_ROUTES, (HOT, inc, gen, HASH_SPACE, 0, 17)).fetchall()
            assert len(routes) == 16
            rows_per_request = 1 + len(routes)
        per_request = len([s for s in statements if "FROM indexes LIMIT" not in s])
    return {"variant": variant, "peak_rss_bytes": rss("VmHWM"), "statements_per_request": per_request,
            "rows_per_request": rows_per_request, "plans": plans}


def experiment(directory: Path, small: int, large: int, repetitions: int, iterations: int) -> dict:
    if platform.system() != "Linux":
        raise SystemExit("needs native Linux")
    directory.mkdir(parents=True, exist_ok=False)
    datasets, samples = [], []
    for idle in (small, large):
        path = directory / f"{idle}.sqlite"
        build(path, idle)
        with closing(sqlite3.connect(path)) as db:
            counts = [db.execute(f"SELECT COUNT(*) FROM {t}").fetchone()[0] for t in ("indexes", "routes")]
        assert counts == [idle + 1, idle + 16], counts
        digest = hashlib.sha256()
        with path.open("rb") as f:
            while chunk := f.read(1 << 20):
                digest.update(chunk)
        datasets.append({"idle_indexes": idle, "index_rows": counts[0], "route_rows": counts[1],
                         "bytes": path.stat().st_size, "sha256": digest.hexdigest()})
        for variant in ("bounded", "full_scan"):
            for repeat in range(repetitions):
                out = subprocess.run([sys.executable, __file__, "probe", "--db", str(path), "--variant", variant,
                                      "--iterations", str(iterations)], capture_output=True, text=True, timeout=300, check=True)
                sample = json.loads(out.stdout)
                sample.update(idle_indexes=idle, repeat=repeat)
                samples.append(sample)
    outcomes = {}
    for variant in ("bounded", "full_scan"):
        groups = [[s for s in samples if s["variant"] == variant and s["idle_indexes"] == n] for n in (small, large)]
        medians = [statistics.median(s["peak_rss_bytes"] for s in g) for g in groups]
        ratio = medians[1] / medians[0]
        indexed = all("SEARCH" in p and "SCAN" not in p for g in groups for s in g for p in s["plans"])
        fixed = all(s["statements_per_request"] == 2 and s["rows_per_request"] == 17 for g in groups for s in g)
        outcomes[variant] = {"gate_passed": indexed and fixed and ratio <= 1.10, "median_peak_rss_bytes": medians,
                             "rss_ratio": ratio, "indexed_plans": indexed, "fixed_selected_work": fixed}
    report = {"profile": "catalog-index-scale-v1", "growth": large // small,
              "scope": "synthetic SQLite model keyed on the index; not the runtime catalog, no physical I/O",
              "source_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "datasets": datasets, "samples": samples, "outcomes": outcomes,
              "passed": outcomes["bounded"]["gate_passed"] and not outcomes["full_scan"]["gate_passed"]}
    with (directory / "report.json").open("x") as f:
        json.dump(report, f, indent=2)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command")
    p = sub.add_parser("probe"); p.add_argument("--db", type=Path, required=True)
    p.add_argument("--variant", default="bounded"); p.add_argument("--iterations", type=int, default=100)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--small", type=int, default=100_000)
    parser.add_argument("--large", type=int, default=1_000_000)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--iterations", type=int, default=100)
    args = parser.parse_args()
    if args.command == "probe":
        print(json.dumps(probe(args.db, args.iterations, args.variant)))
        return 0
    if args.output is None or args.large != args.small * 10:
        parser.error("--output is required and --large must be 10x --small (the model grows 10x per step)")
    report = experiment(args.output, args.small, args.large, args.repetitions, args.iterations)
    print(json.dumps({"passed": report["passed"], "outcomes": report["outcomes"], "datasets": report["datasets"]}, indent=1))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
