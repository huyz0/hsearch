#!/usr/bin/env python3
"""Synthetic bounded-catalog experiment; not a production catalog or wire format.

SQLite supplies a reproducible indexed backend for measuring model work. Its
selection here does not choose a distributed metadata store or certify G01.
"""
from __future__ import annotations

from dataclasses import dataclass
from contextlib import closing
import os
from pathlib import Path
import sqlite3

ROOT = Path(__file__).resolve().parents[1]
HASH_SPACE = 65536  # Deliberately small synthetic routing space, not a product limit.
MAX_CATALOG_BYTES = 128 * 1024**2
MAX_ROUTES = 128


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
    need(len(value.encode("utf-8")) <= 128, "Catalog key exceeds byte budget")
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
        self.connection.execute("PRAGMA query_only=ON")
        self.connection.execute("PRAGMA mmap_size=0")
        self.connection.execute("PRAGMA cache_size=-256")
        self.connection.execute("PRAGMA temp_store=FILE")
        need(self.connection.execute("PRAGMA user_version").fetchone() == (1,), "Unknown model schema")
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
        identity = self._rows("SELECT incarnation,generation FROM collections WHERE tenant=? AND name=?",
                              (tenant, collection), 1)
        need(identity, "Unknown collection")
        incarnation, generation = identity[0]
        rows = self._rows("SELECT partition,lower,upper,reader FROM routes WHERE tenant=? AND name=? AND incarnation=? AND generation=? AND lower<? AND upper>? ORDER BY partition LIMIT ?",
                          (tenant, collection, incarnation, generation, upper, lower, max_selected + 1), max_selected + 1)
        need(len(rows) <= max_selected, "selected_partitions limit exceeded")
        need(rows, "Missing selected route")
        return Selection(tenant, collection, incarnation, generation, tuple(Route(*row) for row in rows))

    def selection_query_plan(self, tenant="synthetic", collection="hot"):
        key(tenant); key(collection)
        return self.connection.execute("EXPLAIN QUERY PLAN SELECT partition FROM routes WHERE tenant=? AND name=? AND incarnation=? AND generation=? ORDER BY partition LIMIT ?",
                                       (tenant, collection, 1, 1, 65)).fetchall()
