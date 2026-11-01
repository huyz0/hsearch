"""Independent synthetic-key and version histories across split/pin/GC transitions."""
from pathlib import Path
import runpy
import tempfile
import unittest

MOD = runpy.run_path(str(Path(__file__).resolve().parents[1] / 'catalog_snapshots.py'))
Catalog, create = MOD['MODEL']['Catalog'], MOD['MODEL']['create_catalog']
Mutable, Pins, Versions, Refused = (MOD[name] for name in ['MutableCatalog', 'Pins', 'Versions', 'Refused'])


class SnapshotModelHistories(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = create(Path(self.directory.name) / 'catalog.sqlite', idle_collections=200)
        self.catalog = Catalog(self.path); self.addCleanup(self.catalog.close)
        self.writer = Mutable(self.catalog); self.addCleanup(self.writer.close)

    def test_pinned_pages_remain_exact_while_split_and_new_keys_publish(self):
        pins = Pins(self.catalog)
        original = [0, 100, 2047, 2048, 4095, 4096, 65535]
        token = pins.open('synthetic', 'hot', original)
        first, cursor = pins.page('synthetic', token, size=2)
        self.assertEqual(first, [0, 100])
        self.writer.split('synthetic', 'hot', expected=(1, 1), partition=0, point=2048)
        original.append(80)
        self.assertEqual(self.writer.gc(pins, 'synthetic', 'hot'), [])
        self.assertEqual(pins.page('synthetic', token, size=2)[0], first)
        collected = first[:]
        while cursor is not None:
            page, cursor = pins.page('synthetic', token, cursor=cursor, size=2)
            collected.extend(page)
        self.assertEqual(collected, [0, 100, 2047, 2048, 4095, 4096, 65535])
        current = pins.open('synthetic', 'hot', original)
        self.assertEqual(pins.page('synthetic', current, size=100)[0], sorted(original))
        with self.assertRaisesRegex(Refused, 'cursor_scope'):
            pins.page('synthetic', current, cursor=MOD['Cursor'](token, 100))
        pins.release('synthetic', token)
        self.assertEqual(self.writer.gc(pins, 'synthetic', 'hot'), [(1, 1)])

    def test_prepared_pin_fence_rejects_split_gc_race_without_partial_publication(self):
        pins = Pins(self.catalog)
        preparation = pins.prepare('synthetic', 'hot', [1, 42])
        self.writer.split('synthetic', 'hot', expected=(1, 1), partition=0, point=2048)
        self.assertEqual(self.writer.gc(pins, 'synthetic', 'hot'), [(1, 1)])
        with self.assertRaisesRegex(Refused, 'stale_snapshot_preparation'): pins.commit('synthetic', preparation)
        self.assertEqual(pins.active, {})
        self.assertEqual(pins.prepared, {})

    def test_expiry_is_typed_and_does_not_serve_a_new_generation(self):
        pins = Pins(self.catalog)
        token = pins.open('synthetic', 'hot', [1, 2], ttl=4)
        self.writer.split('synthetic', 'hot', expected=(1, 1), partition=0, point=2048)
        pins.advance(4)
        with self.assertRaisesRegex(Refused, 'snapshot_expired'): pins.page('synthetic', token)
        self.assertEqual(pins.references('synthetic', 'hot'), set())
        self.assertEqual(self.writer.gc(pins, 'synthetic', 'hot'), [(1, 1)])

    def test_snapshot_count_and_byte_limit_plus_one_preserve_existing_pins(self):
        pins = Pins(self.catalog)
        first = pins.open('synthetic', 'hot', [1]); second = pins.open('synthetic', 'hot', [2])
        with self.assertRaisesRegex(Refused, 'active_snapshot_quota'): pins.open('synthetic', 'hot', [3])
        self.assertEqual(set(pins.active), {first, second})
        cost = pins.active[first].charged_bytes
        bounded = Pins(self.catalog, tenant_bytes=2 * cost - 1, global_bytes=4 * cost - 2)
        safe = bounded.open('synthetic', 'hot', [1])
        with self.assertRaisesRegex(Refused, 'snapshot_pin_bytes'): bounded.open('synthetic', 'hot', [2])
        self.assertEqual(set(bounded.active), {safe})
        self.assertEqual(bounded.prepared, {})

    def test_revocation_and_cross_tenant_access_never_release_another_owner(self):
        self.writer.create('other', 'hot')
        pins = Pins(self.catalog)
        a = pins.open('synthetic', 'hot', [1], key_generation=3)
        b = pins.open('other', 'hot', [1], key_generation=3)
        with self.assertRaisesRegex(Refused, 'tenant_mismatch'): pins.release('other', a)
        pins.revoke('synthetic', 3)
        with self.assertRaisesRegex(Refused, 'key_revoked'): pins.page('synthetic', a)
        self.assertEqual(pins.page('other', b)[0], [1])
        with self.assertRaisesRegex(Refused, 'key_revoked'): pins.open('synthetic', 'hot', [1], key_generation=3)

    def test_versions_survive_split_and_reject_stale_or_aba_mutations(self):
        versions = Versions(self.catalog)
        first = versions.replace('synthetic', 'hot', 42)
        self.writer.split('synthetic', 'hot', expected=(1, 1), partition=0, point=2048)
        second = versions.replace('synthetic', 'hot', 42, expected=first)
        self.assertEqual(second.version, 2)
        with self.assertRaisesRegex(Refused, 'version_conflict'): versions.replace('synthetic', 'hot', 42, expected=first)
        before = dict(versions.values)
        with self.assertRaisesRegex(Refused, 'mutation_preparation_failed'):
            versions.replace('synthetic', 'hot', 42, expected=second, preparation_valid=False)
        self.assertEqual(versions.values, before)
        pins = Pins(self.catalog); snapshot = pins.open('synthetic', 'hot', [42])
        self.writer.recreate('synthetic', 'hot', expected=(1, 2))
        with self.assertRaisesRegex(Refused, 'version_conflict'): versions.replace('synthetic', 'hot', 42, expected=second)
        with self.assertRaisesRegex(Refused, 'incarnation_changed'): pins.page('synthetic', snapshot)
        replacement = versions.replace('synthetic', 'hot', 42)
        self.assertEqual((replacement.incarnation, replacement.version), (2, 1))

    def test_invalid_or_stale_split_has_no_partial_generation(self):
        for options in [{'expected': (1, 1), 'partition': 0, 'point': 0}, {'expected': (1, 2), 'partition': 0, 'point': 2048},
                        {'expected': (True, 1), 'partition': 0, 'point': 2048}]:
            with self.assertRaises(Refused): self.writer.split('synthetic', 'hot', **options)
            self.assertEqual(self.catalog.identity('synthetic', 'hot'), (1, 1))
            self.assertFalse(self.catalog.generation_exists('synthetic', 'hot', 1, 2))

    def test_retention_refuses_growth_instead_of_expiring_live_pins(self):
        pins = Pins(self.catalog)
        token = pins.open('synthetic', 'hot', [1], ttl=900)
        # Recreate is a model authority transition; old rows remain until explicit GC.
        expected = (1, 1)
        for _ in range(15): expected = self.writer.recreate('synthetic', 'hot', expected=expected)
        with self.assertRaisesRegex(Refused, 'retained_generation_limit'):
            self.writer.recreate('synthetic', 'hot', expected=expected)
        self.assertIn(token, pins.active)
        with self.assertRaisesRegex(Refused, 'incarnation_changed'): pins.page('synthetic', token)
        self.assertEqual(len(self.writer.gc(pins, 'synthetic', 'hot')), 15)
