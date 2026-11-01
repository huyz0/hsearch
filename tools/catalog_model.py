#!/usr/bin/env python3
"""Synthetic bounded-catalog experiment; not a production catalog or wire format.

SQLite supplies a reproducible indexed backend for measuring model work. Its
selection here does not choose a distributed metadata store or certify G01.
"""
from __future__ import annotations

from dataclasses import dataclass
from contextlib import closing
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import sqlite3
import statistics
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
HASH_SPACE = 65536  # Deliberately small synthetic routing space, not a product limit.
MAX_CATALOG_BYTES = 128 * 1024**2
MAX_ROUTES = 128
IDENTITY_SQL = "SELECT incarnation,generation FROM collections WHERE tenant=? AND name=?"
ROUTE_SQL = "SELECT partition,lower,upper,reader FROM routes WHERE tenant=? AND name=? AND incarnation=? AND generation=? AND lower<? AND upper>? ORDER BY partition LIMIT ?"


class Refused(ValueError):
    pass


def need(condition, message):
    if not condition:
        raise Refused(message)


def integer(value, lower, upper, name):
    need(type(value) is int and lower <= value <= upper, "Invalid finite integer: " + name)
    return value


def key(value):
    need(type(value) is str and 0 < len(value) <= 128 and
         all(ord(char) >= 32 for char in value), "Invalid bounded catalog key")
    try:
        encoded = value.encode("utf-8")
    except UnicodeEncodeError:
        raise Refused("Catalog key is not valid UTF-8") from None
    need(len(encoded) <= 128, "Catalog key exceeds byte budget")
    return value


@dataclass(frozen=True)
class Route:
    partition: int
    lower: int
    upper: int
    reader: int


@dataclass(frozen=True)
class Selection:
    tenant: str
    collection: str
    incarnation: int
    generation: int
    routes: tuple[Route, ...]


def create_catalog(path, *, idle_collections, selected_routes=16):
    """Build an original public fixture in bounded batches, preserving failures."""
    path = Path(path).absolute()
    need(path.resolve() == path and not path.is_relative_to(ROOT), "Fixture must be unredirected and outside source")
    integer(idle_collections, 0, 100000, "idle collections")
    integer(selected_routes, 1, MAX_ROUTES, "routes")
    descriptor = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    os.close(descriptor)
    with closing(sqlite3.connect(path)) as connection:
        connection.executescript("""
            PRAGMA journal_mode=DELETE;
            PRAGMA mmap_size=0;
            PRAGMA cache_size=-256;
            CREATE TABLE collections (
                tenant TEXT NOT NULL, name TEXT NOT NULL,
                incarnation INTEGER NOT NULL, generation INTEGER NOT NULL,
                PRIMARY KEY (tenant, name)
            ) WITHOUT ROWID;
            CREATE TABLE routes (
                tenant TEXT NOT NULL, name TEXT NOT NULL,
                incarnation INTEGER NOT NULL, generation INTEGER NOT NULL,
                partition INTEGER NOT NULL, lower INTEGER NOT NULL,
                upper INTEGER NOT NULL, reader INTEGER NOT NULL,
                PRIMARY KEY (tenant, name, incarnation, generation, partition)
            ) WITHOUT ROWID;
            PRAGMA user_version=1;
        """)
        connection.execute("INSERT INTO collections VALUES (?,?,1,1)", ("synthetic", "hot"))
        connection.executemany("INSERT INTO routes VALUES (?,?,1,1,?,?,?,1)",
            (("synthetic", "hot", index, index * HASH_SPACE // selected_routes,
              (index + 1) * HASH_SPACE // selected_routes) for index in range(selected_routes)))
        for start in range(0, idle_collections, 128):
            names = ["idle-" + str(index).zfill(8) for index in range(start, min(start + 128, idle_collections))]
            connection.executemany("INSERT INTO collections VALUES (?,?,1,1)", (("synthetic", name) for name in names))
            connection.executemany("INSERT INTO routes VALUES (?,?,1,1,0,0,?,1)", (("synthetic", name, HASH_SPACE) for name in names))
            need(path.stat().st_size <= MAX_CATALOG_BYTES, "Synthetic catalog disk budget exceeded")
        connection.commit()
    need(path.stat().st_size <= MAX_CATALOG_BYTES, "Synthetic catalog disk budget exceeded")
    return path


class Catalog:
    """One bounded connection; no per-idle-collection Python objects or timers."""
    def __init__(self, path):
        path = Path(path).absolute()
        need(path.resolve() == path and path.is_file() and 0 < path.stat().st_size <= MAX_CATALOG_BYTES,
             "Missing, redirected or oversized model catalog")
        self.connection = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)
        self.path = path
        self.file_identity = (path.stat().st_dev, path.stat().st_ino)
        try:
            self.connection.execute("PRAGMA query_only=ON")
            self.connection.execute("PRAGMA mmap_size=0")
            self.connection.execute("PRAGMA cache_size=-256")
            self.connection.execute("PRAGMA temp_store=FILE")
            need(self.connection.execute("PRAGMA user_version").fetchone() == (1,), "Unknown model schema")
        except BaseException:
            self.connection.close()
            raise
        self.metadata_statements = 0  # SQL statements, not object-store/page-read attempts.
        self.returned_rows = 0
        self.connection.set_trace_callback(self._trace)

    def _trace(self, statement):
        if statement.lstrip().upper().startswith("SELECT "):
            self.metadata_statements += 1

    def close(self):
        self.connection.set_trace_callback(None)
        self.connection.close()

    def __enter__(self):
        return self

    def __exit__(self, *_error):
        self.close()

    def _rows(self, statement, arguments, bound):
        rows = self.connection.execute(statement, arguments).fetchmany(bound + 1)
        need(len(rows) <= bound, "Catalog result exceeded admitted row bound")
        self.returned_rows += len(rows)
        return rows

    def page(self, tenant, *, after="", limit=128):
        key(tenant)
        need(type(after) is str, "Invalid catalog cursor type")
        if after:
            key(after)
        integer(limit, 1, 1000, "page size")
        rows = self._rows("SELECT name,incarnation,generation FROM collections WHERE tenant=? AND name>? ORDER BY name LIMIT ?",
                          (tenant, after, limit + 1), limit + 1)
        return rows[:limit], rows[limit - 1][0] if len(rows) > limit else None

    def select(self, tenant, collection, *, lower=0, upper=HASH_SPACE, max_selected=64):
        key(tenant); key(collection)
        integer(lower, 0, HASH_SPACE - 1, "range lower")
        integer(upper, lower + 1, HASH_SPACE, "range upper")
        integer(max_selected, 1, MAX_ROUTES, "selected partitions")
        identity = self._rows(IDENTITY_SQL,
                              (tenant, collection), 1)
        need(identity, "Unknown collection")
        incarnation, generation = identity[0]
        rows = self._rows(ROUTE_SQL,
                          (tenant, collection, incarnation, generation, upper, lower, max_selected + 1), max_selected + 1)
        need(len(rows) <= max_selected, "selected_partitions limit exceeded")
        need(rows, "Missing selected route")
        return Selection(tenant, collection, incarnation, generation, tuple(Route(*row) for row in rows))

    def selection_query_plan(self, tenant="synthetic", collection="hot"):
        key(tenant); key(collection)
        return [*self.connection.execute("EXPLAIN QUERY PLAN " + IDENTITY_SQL, (tenant, collection)).fetchall(),
                *self.connection.execute("EXPLAIN QUERY PLAN " + ROUTE_SQL, (tenant, collection, 1, 1, HASH_SPACE, 0, 65)).fetchall()]

    def identity(self, tenant, collection):
        key(tenant); key(collection)
        need((self.path.stat().st_dev, self.path.stat().st_ino) == self.file_identity, 'catalog_replaced')
        rows = self._rows(IDENTITY_SQL, (tenant, collection), 1)
        need(rows, 'unknown_collection')
        return tuple(rows[0])

    def generation_exists(self, tenant, collection, incarnation, generation):
        key(tenant); key(collection)
        integer(incarnation, 1, 2**31 - 1, 'incarnation'); integer(generation, 1, 2**31 - 1, 'generation')
        return bool(self._rows('SELECT 1 FROM routes WHERE tenant=? AND name=? AND incarnation=? AND generation=? LIMIT 1',
                               (tenant, collection, incarnation, generation), 1))


def rss_bytes():
    need(platform.system() == "Linux", "RSS model experiment requires native Linux")
    status = Path('/proc/self/status').read_text(encoding='ascii')
    need(len(status) < 65536, "Process status exceeds bound")
    for line in status.splitlines():
        if line.startswith('VmRSS:'):
            value, units = line.split()[1:]
            need(units == 'kB' and value.isdecimal(), "Unknown RSS units")
            return int(value) * 1024
    raise Refused('RSS unavailable')


def probe(path, *, iterations=100, variant='bounded'):
    """Fresh-process synthetic measurement; deliberately bad variants are bounded."""
    integer(iterations, 1, 100, 'probe iterations')
    need(variant in {'bounded', 'full_scan', 'timer_records'}, 'Unknown model variant')
    before = rss_bytes()
    retained, timer_records, maximum_rows = [], 0, 0
    with Catalog(path) as catalog:
        for _ in range(5): catalog.select('synthetic', 'hot')
        plans = [row[3] for row in catalog.selection_query_plan()]
        if variant != 'bounded':
            plans += [row[3] for row in catalog.connection.execute('EXPLAIN QUERY PLAN SELECT tenant,name FROM collections LIMIT 100001')]
        catalog.metadata_statements = catalog.returned_rows = 0
        started = time.perf_counter()
        for _ in range(iterations):
            rows_before = catalog.returned_rows
            if variant != 'bounded':
                retained = catalog._rows('SELECT tenant,name FROM collections LIMIT 100001', (), 100001)
                if variant == 'timer_records':
                    # Model allocation only; never spawn a timer/thread per idle collection.
                    retained = [(tenant, name, 0) for tenant, name in retained]
                    timer_records = len(retained)
            selected = catalog.select('synthetic', 'hot')
            need(len(selected.routes) == 16, 'Selected work differs from original fixture')
            maximum_rows = max(maximum_rows, catalog.returned_rows - rows_before)
        elapsed = time.perf_counter() - started
        after = rss_bytes()
        return {'schema_version': 1, 'scope': 'synthetic SQLite model; not hsearch runtime/fleet G01 qualification',
                'variant': variant, 'iterations': iterations, 'selected_partitions': 16,
                'rss_before_catalog_bytes': before, 'rss_after_hot_work_bytes': after,
                'sql_statement_attempts': catalog.metadata_statements, 'returned_rows': catalog.returned_rows,
                'maximum_rows_per_request': maximum_rows, 'timer_model_records': timer_records,
                'retained_negative_control_rows': len(retained), 'query_plans': plans,
                'elapsed_seconds': elapsed, 'python': platform.python_version(), 'sqlite': sqlite3.sqlite_version,
                'kernel': platform.release(), 'sqlite_cache_kib': 256, 'mmap_bytes': 0}


def scale_experiment(directory, *, small=1000, large=10000, repetitions=3, iterations=100):
    need(platform.system() == 'Linux', 'Fresh-process scale experiment requires native Linux')
    integer(small, 1, 10000, 'small catalog'); integer(large, 10, 100000, 'large catalog')
    integer(repetitions, 3, 10, 'repetitions'); integer(iterations, 1, 100, 'iterations')
    need(large == small * 10, 'Experiment must grow idle cardinality exactly10x')
    directory = Path(directory).absolute()
    need(directory.resolve() == directory and not directory.is_relative_to(ROOT), 'Experiment output must be private and outside source')
    directory.mkdir(parents=True, exist_ok=False)
    datasets, samples = [], []
    for idle in [small, large]:
        path = create_catalog(directory / (str(idle) + '.sqlite'), idle_collections=idle)
        with closing(sqlite3.connect(path)) as connection:
            counts = [connection.execute('SELECT COUNT(*) FROM ' + table).fetchone()[0] for table in ['collections', 'routes']]
        need(counts == [idle + 1, idle + 16], 'Setup-only actual fixture cardinality mismatch')
        digest = hashlib.sha256()
        with path.open('rb') as source:
            while chunk := source.read(65536): digest.update(chunk)
        datasets.append({'idle_collections': idle, 'collection_rows': counts[0], 'route_rows': counts[1],
                         'bytes': path.stat().st_size, 'sha256': digest.hexdigest()})
        for variant in ['bounded', 'full_scan', 'timer_records']:
            for repeat in range(repetitions):
                result = subprocess.run([sys.executable, str(Path(__file__).resolve()), 'probe', '--catalog', str(path),
                                         '--iterations', str(iterations), '--variant', variant],
                                        text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
                need(result.returncode == 0 and len(result.stdout.encode()) <= 16384 and len(result.stderr.encode()) <= 16384,
                     'Fresh process probe failed or exceeded output bound')
                sample = json.loads(result.stdout)
                sample.update(idle_collections=idle, repeat=repeat)
                samples.append(sample)
    outcomes = {}
    for variant in ['bounded', 'full_scan', 'timer_records']:
        groups = [[sample for sample in samples if sample['variant'] == variant and sample['idle_collections'] == idle] for idle in [small, large]]
        medians = [statistics.median(sample['rss_after_hot_work_bytes'] for sample in group) for group in groups]
        ratio = medians[1] / medians[0]
        no_scan = all(all('SEARCH' in plan and 'SCAN' not in plan for plan in sample['query_plans']) for group in groups for sample in group)
        bounded_work = all(sample['maximum_rows_per_request'] == 17 and sample['timer_model_records'] == 0 and
                           sample['sql_statement_attempts'] == iterations * 2 for group in groups for sample in group)
        outcomes[variant] = {'model_gate_passed': no_scan and bounded_work and ratio <= 1.10,
                             'median_hot_rss_bytes': medians, 'rss_ratio': ratio, 'indexed_plans': no_scan,
                             'fixed_selected_work': bounded_work}
    report = {'schema_version': 1, 'scope': 'model evidence only; API G01 remains unobserved for hsearch runtime',
              'model_source_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'datasets': datasets, 'samples': samples, 'outcomes': outcomes,
              'counts_note': 'SQL statement attempts and returned rows; not object-store or physical page-read attempts',
              'rss_note': 'Fresh-process absolute hot RSS includes Python/SQLite; raw baseline and repeat noise remain visible',
              'passed': outcomes['bounded']['model_gate_passed'] and not outcomes['full_scan']['model_gate_passed'] and not outcomes['timer_records']['model_gate_passed']}
    encoded = json.dumps(report, indent=2).encode('utf-8')
    need(len(encoded) <= 1024**2, 'Model report exceeds bound')
    with (directory / 'report.json').open('xb') as output: output.write(encoded + b'\n')
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    one = sub.add_parser('probe'); one.add_argument('--catalog', type=Path, required=True)
    one.add_argument('--iterations', type=int, default=100); one.add_argument('--variant', default='bounded')
    scale = sub.add_parser('scale'); scale.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.command == 'probe': print(json.dumps(probe(args.catalog, iterations=args.iterations, variant=args.variant)))
    else:
        report = scale_experiment(args.output)
        print(json.dumps({'catalog_model_scale': 'pass' if report['passed'] else 'fail', 'outcomes': report['outcomes'], 'evidence': str(args.output / 'report.json')}))
        return 0 if report['passed'] else 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
