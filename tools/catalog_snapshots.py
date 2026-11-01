"""Serial split/snapshot/GC model over synthetic routing and immutable key views.

This models fences and quotas, not a search engine, token cryptography, durable
mutations or distributed atomicity. Keys stand in for independent reader views.
"""
from dataclasses import dataclass
from pathlib import Path
import json
import runpy
import sqlite3

MODEL = runpy.run_path(str(Path(__file__).with_name('catalog_model.py')))
need, integer, key, Refused = (MODEL[name] for name in ['need', 'integer', 'key', 'Refused'])


class MutableCatalog:
    def __init__(self, catalog):
        catalog.guard_identity()
        self.catalog = catalog
        self.path = catalog.path
        self.connection = sqlite3.connect(self.path.as_uri() + '?mode=rw', uri=True)
        self.connection.execute('PRAGMA cache_size=-256')

    def close(self): self.connection.close()
    def __enter__(self): return self
    def __exit__(self, *_error): self.close()

    def _begin(self, tenant, collection, expected):
        key(tenant); key(collection)
        need(type(expected) is tuple and len(expected) == 2, 'identity_shape')
        for value in expected: integer(value, 1, 2**31 - 1, 'identity')
        self._prepare_write()
        self.connection.execute('BEGIN IMMEDIATE')
        current = self.connection.execute(MODEL['IDENTITY_SQL'], (tenant, collection)).fetchone()
        need(current == expected, 'stale_routing_or_incarnation')
        retained = self.connection.execute('SELECT DISTINCT incarnation,generation FROM routes WHERE tenant=? AND name=? LIMIT 17', (tenant, collection)).fetchall()
        need(len(retained) < 16, 'retained_generation_limit')
        return current

    def _prepare_write(self):
        self.catalog.guard_identity()
        maximum = integer(MODEL['MAX_CATALOG_BYTES'], 512, 128 * 1024**2, 'catalog_disk_budget')
        need(self.path.stat().st_size <= maximum, 'catalog_disk_budget')
        page_size = self.connection.execute('PRAGMA page_size').fetchone()[0]
        pages = maximum // page_size
        need(pages > 0 and self.connection.execute('PRAGMA max_page_count=' + str(pages)).fetchone()[0] <= pages,
             'catalog_disk_budget')

    def _rollback(self, error):
        self.connection.rollback()
        if isinstance(error, sqlite3.Error) and getattr(error, 'sqlite_errorcode', None) == sqlite3.SQLITE_FULL:
            raise Refused('catalog_disk_budget') from error

    def create(self, tenant, collection):
        self._prepare_write()
        key(tenant); key(collection)
        try:
            self.connection.execute('BEGIN IMMEDIATE')
            need(self.connection.execute(MODEL['IDENTITY_SQL'], (tenant, collection)).fetchone() is None, 'collection_exists')
            self.connection.execute('INSERT INTO collections VALUES (?,?,1,1)', (tenant, collection))
            self.connection.execute('INSERT INTO routes VALUES (?,?,1,1,0,0,?,1)', (tenant, collection, MODEL['HASH_SPACE']))
            need(self.path.stat().st_size <= MODEL['MAX_CATALOG_BYTES'], 'catalog_disk_budget')
            self.connection.commit()
        except BaseException as error:
            self._rollback(error); raise

    def split(self, tenant, collection, *, expected, partition, point):
        integer(partition, 0, 2**31 - 1, 'partition'); integer(point, 1, MODEL['HASH_SPACE'] - 1, 'split_point')
        try:
            incarnation, generation = self._begin(tenant, collection, expected)
            rows = self.connection.execute('SELECT partition,lower,upper,reader FROM routes WHERE tenant=? AND name=? AND incarnation=? AND generation=? ORDER BY lower LIMIT 129',
                                           (tenant, collection, incarnation, generation)).fetchall()
            need(0 < len(rows) < MODEL['MAX_ROUTES'], 'active_route_limit')
            need(rows[0][1] == 0 and rows[-1][2] == MODEL['HASH_SPACE'] and
                 all(left[2] == right[1] for left, right in zip(rows, rows[1:])), 'route_gap_or_overlap')
            parent = next((row for row in rows if row[0] == partition), None)
            need(parent is not None and parent[1] < point < parent[2], 'invalid_split')
            integer(generation + 1, 1, 2**31 - 1, 'generation_overflow')
            first = max(row[0] for row in rows) + 1
            integer(first + 1, 0, 2**31 - 1, 'partition_overflow')
            new = [row for row in rows if row[0] != partition] + [(first, parent[1], point, generation + 1), (first + 1, point, parent[2], generation + 1)]
            self.connection.executemany('INSERT INTO routes VALUES (?,?,?,?,?,?,?,?)',
                ((tenant, collection, incarnation, generation + 1, *row) for row in new))
            self.connection.execute('UPDATE collections SET generation=? WHERE tenant=? AND name=?', (generation + 1, tenant, collection))
            need(self.path.stat().st_size <= MODEL['MAX_CATALOG_BYTES'], 'catalog_disk_budget')
            self.connection.commit()
            return incarnation, generation + 1
        except BaseException as error:
            self._rollback(error)
            raise

    def recreate(self, tenant, collection, *, expected):
        try:
            incarnation, _generation = self._begin(tenant, collection, expected)
            integer(incarnation + 1, 1, 2**31 - 1, 'incarnation_overflow')
            self.connection.execute('INSERT INTO routes VALUES (?,?,?,1,0,0,?,1)', (tenant, collection, incarnation + 1, MODEL['HASH_SPACE']))
            self.connection.execute('UPDATE collections SET incarnation=?,generation=1 WHERE tenant=? AND name=?', (incarnation + 1, tenant, collection))
            self.connection.commit()
            return incarnation + 1, 1
        except BaseException as error:
            self._rollback(error); raise

    def gc(self, pins, tenant, collection):
        self._prepare_write()
        need(pins is self.catalog.pin_registry and pins.catalog is self.catalog, 'foreign_pin_registry')
        key(tenant); key(collection)
        protected = pins.references(tenant, collection)
        try:
            self.connection.execute('BEGIN IMMEDIATE')
            current = self.connection.execute(MODEL['IDENTITY_SQL'], (tenant, collection)).fetchone()
            need(current is not None, 'unknown_collection')
            generations = self.connection.execute('SELECT DISTINCT incarnation,generation FROM routes WHERE tenant=? AND name=? LIMIT 17', (tenant, collection)).fetchall()
            need(len(generations) <= 16, 'retained_generation_limit')
            removed = []
            for generation in generations:
                if generation != current and generation not in protected:
                    self.connection.execute('DELETE FROM routes WHERE tenant=? AND name=? AND incarnation=? AND generation=?', (tenant, collection, *generation))
                    removed.append(generation)
            self.connection.commit()
            return removed
        except BaseException as error:
            self._rollback(error); raise


@dataclass(frozen=True)
class Cursor:
    snapshot: str
    after: int


@dataclass(frozen=True)
class View:
    tenant: str
    collection: str
    incarnation: int
    generation: int
    routes: tuple
    keys: tuple
    key_generation: int
    expires: int
    charged_bytes: int


class Pins:
    def __init__(self, catalog, *, tenants=('synthetic', 'other'), tenant_limit=2, global_limit=4,
                 tenant_bytes=8192, global_bytes=16384, max_ttl=900):
        catalog.guard_identity()
        need(catalog.pin_registry is None, 'model_pin_authority_conflict')
        need(type(tenants) is tuple and 0 < len(tenants) <= 64 and len(set(tenants)) == len(tenants), 'tenant_policy')
        for tenant in tenants: key(tenant)
        for name, value in [('tenant_limit', tenant_limit), ('global_limit', global_limit), ('tenant_bytes', tenant_bytes), ('global_bytes', global_bytes), ('max_ttl', max_ttl)]:
            integer(value, 1, 2**31 - 1, name)
        need(global_limit <= 128 and tenant_limit * len(tenants) <= global_limit and tenant_bytes * len(tenants) <= global_bytes, 'reserved_snapshot_quota')
        self.catalog, self.tenants = catalog, tenants
        self.tenant_limit, self.global_limit = tenant_limit, global_limit
        self.tenant_bytes, self.global_bytes, self.max_ttl = tenant_bytes, global_bytes, max_ttl
        self.prepared, self.active, self.terminal = {}, {}, {}
        self.revoked_through = dict.fromkeys(tenants, 0)
        self.now = self.sequence = 0
        catalog.pin_registry = self

    def _owner(self, tenant):
        self.catalog.guard_identity()
        need(self.catalog.pin_registry is self, 'foreign_pin_registry')
        key(tenant); need(tenant in self.tenants, 'unknown_tenant')

    def prepare(self, tenant, collection, keys, *, ttl=100, key_generation=1):
        self._owner(tenant); key(collection)
        integer(ttl, 1, self.max_ttl, 'snapshot_ttl'); integer(key_generation, 1, 2**31 - 1, 'key_generation')
        need(key_generation > self.revoked_through[tenant], 'key_revoked')
        need(type(keys) in {list, tuple} and len(keys) <= 1024, 'snapshot_key_bound')
        for value in keys: integer(value, 0, MODEL['HASH_SPACE'] - 1, 'document_key')
        need(len(set(keys)) == len(keys), 'duplicate_document_key')
        reserved = [*self.active.values(), *self.prepared.values()]
        need(len(reserved) < self.global_limit and sum(view.tenant == tenant for view in reserved) < self.tenant_limit, 'active_snapshot_quota')
        selection = self.catalog.select(tenant, collection)
        frozen = tuple(sorted(keys))
        integer(self.sequence + 1, 1, 2**63 - 1, 'snapshot_identity_overflow')
        identity = 'snapshot-' + self.catalog.authority_nonce + '-' + str(self.sequence + 1)
        encoded = json.dumps([tenant, collection, selection.incarnation, selection.generation,
                              [[route.partition, route.lower, route.upper, route.reader] for route in selection.routes], frozen,
                              key_generation, self.now + ttl, identity], separators=(',', ':')).encode()
        need(sum(view.charged_bytes for view in reserved) + len(encoded) <= self.global_bytes and
             sum(view.charged_bytes for view in reserved if view.tenant == tenant) + len(encoded) <= self.tenant_bytes, 'snapshot_pin_bytes')
        integer(self.now + ttl, 0, 2**63 - 1, 'expiry_overflow')
        self.sequence += 1
        self.prepared[identity] = View(tenant, collection, selection.incarnation, selection.generation,
                                      selection.routes, frozen, key_generation, self.now + ttl, len(encoded))
        return identity

    def commit(self, tenant, identity):
        self._owner(tenant); key(identity)
        need(identity in self.prepared, 'unknown_preparation')
        view = self.prepared[identity]
        need(view.tenant == tenant, 'tenant_mismatch')
        try:
            need(view.expires > self.now, 'snapshot_expired')
            need(view.key_generation > self.revoked_through[tenant], 'key_revoked')
            need(self.catalog.identity(tenant, view.collection) == (view.incarnation, view.generation) and
                 self.catalog.generation_exists(tenant, view.collection, view.incarnation, view.generation), 'stale_snapshot_preparation')
            need(len(self.active) < self.global_limit and sum(item.tenant == tenant for item in self.active.values()) < self.tenant_limit, 'active_snapshot_quota')
            need(sum(item.charged_bytes for item in self.active.values()) + view.charged_bytes <= self.global_bytes and
                 sum(item.charged_bytes for item in self.active.values() if item.tenant == tenant) + view.charged_bytes <= self.tenant_bytes, 'snapshot_pin_bytes')
        except BaseException:
            del self.prepared[identity]
            raise
        self.active[identity] = self.prepared.pop(identity)
        return identity

    def open(self, tenant, collection, keys, **options):
        return self.commit(tenant, self.prepare(tenant, collection, keys, **options))

    def _release(self, identity, reason):
        view = self.active.pop(identity)
        if len(self.terminal) >= 128: del self.terminal[next(iter(self.terminal))]
        self.terminal[identity] = (view.tenant, reason)

    def get(self, tenant, identity):
        self._owner(tenant); key(identity)
        if identity in self.terminal:
            need(self.terminal[identity][0] == tenant, 'tenant_mismatch')
            raise Refused(self.terminal[identity][1])
        need(identity in self.active, 'snapshot_unknown_or_receipt_expired')
        view = self.active[identity]
        need(view.tenant == tenant, 'tenant_mismatch')
        reason = None
        if view.expires <= self.now: reason = 'snapshot_expired'
        elif view.key_generation <= self.revoked_through[tenant]: reason = 'key_revoked'
        elif self.catalog.identity(tenant, view.collection)[0] != view.incarnation: reason = 'incarnation_changed'
        elif not self.catalog.generation_exists(tenant, view.collection, view.incarnation, view.generation): reason = 'snapshot_unavailable'
        if reason:
            self._release(identity, reason); raise Refused(reason)
        return view

    def page(self, tenant, identity, *, cursor=None, size=3):
        integer(size, 1, 1000, 'page_size')
        view = self.get(tenant, identity)
        if cursor is not None:
            need(type(cursor) is Cursor and cursor.snapshot == identity, 'cursor_scope')
            integer(cursor.after, 0, MODEL['HASH_SPACE'] - 1, 'cursor_key')
            need(cursor.after in view.keys, 'cursor_position')
        remaining = [value for value in view.keys if cursor is None or value > cursor.after]
        result = remaining[:size]
        for value in result: need(sum(route.lower <= value < route.upper for route in view.routes) == 1, 'snapshot_route_gap')
        return result, Cursor(identity, result[-1]) if len(remaining) > size else None

    def release(self, tenant, identity):
        self.get(tenant, identity); self._release(identity, 'released')

    def advance(self, ticks):
        integer(ticks, 0, self.max_ttl, 'clock_step'); integer(self.now + ticks, 0, 2**63 - 1, 'clock_overflow')
        self.now += ticks
        for identity, view in tuple(self.active.items()):
            if view.expires <= self.now: self._release(identity, 'snapshot_expired')
        for identity, view in tuple(self.prepared.items()):
            if view.expires <= self.now: del self.prepared[identity]

    def revoke(self, tenant, key_generation):
        self._owner(tenant); integer(key_generation, 1, 2**31 - 1, 'key_generation')
        # Symbolic policy-epoch fence: revokes this epoch and all older epochs.
        self.revoked_through[tenant] = max(self.revoked_through[tenant], key_generation)
        for identity, view in tuple(self.active.items()):
            if view.tenant == tenant and view.key_generation <= self.revoked_through[tenant]: self._release(identity, 'key_revoked')

    def references(self, tenant, collection):
        self._owner(tenant); key(collection)
        return {(view.incarnation, view.generation) for view in self.active.values() if view.tenant == tenant and view.collection == collection}


@dataclass(frozen=True)
class Version:
    tenant: str
    collection: str
    incarnation: int
    document: int
    version: int
    authority: str


class Versions:
    """Symbolic CAS history; no mutation durability or native token format."""
    def __init__(self, catalog): self.catalog, self.values = catalog, {}

    def replace(self, tenant, collection, document, *, expected=None, preparation_valid=True):
        key(tenant); key(collection); integer(document, 0, MODEL['HASH_SPACE'] - 1, 'document_key')
        incarnation, _generation = self.catalog.identity(tenant, collection)
        identity = (tenant, collection, incarnation, document)
        version = self.values.get(identity, 0)
        need(type(preparation_valid) is bool and preparation_valid, 'mutation_preparation_failed')
        if expected is not None:
            need(type(expected) is Version, 'version_token')
            for value in [expected.incarnation, expected.version]: integer(value, 1, 2**31 - 1, 'version_token')
            integer(expected.document, 0, MODEL['HASH_SPACE'] - 1, 'version_token')
            need(expected == Version(tenant, collection, incarnation, document, version, self.catalog.authority_nonce), 'version_conflict')
        need(identity in self.values or len(self.values) < 1024, 'version_history_bound')
        integer(version + 1, 1, 2**31 - 1, 'version_overflow')
        self.values[identity] = version + 1
        return Version(tenant, collection, incarnation, document, version + 1, self.catalog.authority_nonce)
