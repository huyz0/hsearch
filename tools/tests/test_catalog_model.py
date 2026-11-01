"""Original indexed-catalog controls; these do not certify runtime scalability."""
from pathlib import Path
import runpy
import tempfile
import unittest
import os
import platform
import json

MODEL = runpy.run_path(str(Path(__file__).resolve().parents[1] / "catalog_model.py"))


class CatalogModelControls(unittest.TestCase):
    @unittest.skipUnless(platform.system() == 'Linux', 'Fresh-process RSS experiment requires native Linux')
    def test_actual_tenfold_idle_experiment_and_full_scan_timer_counterexamples(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get('CARGO_TARGET_DIR')) as directory:
            report = MODEL['scale_experiment'](Path(directory) / 'evidence', small=100, large=1000, repetitions=3, iterations=10)
            self.assertEqual(len(report['samples']), 18)
            self.assertEqual([dataset['collection_rows'] for dataset in report['datasets']], [101, 1001])
            self.assertTrue(report['passed'])
            self.assertTrue(report['outcomes']['bounded']['model_gate_passed'])
            self.assertFalse(report['outcomes']['full_scan']['model_gate_passed'])
            self.assertFalse(report['outcomes']['timer_records']['model_gate_passed'])
            print(json.dumps({'catalog_model_experiment': report}), flush=True)

    def test_fixed_selection_and_metadata_statements_do_not_enumerate_idle_catalog(self):
        with tempfile.TemporaryDirectory() as temporary:
            measurements = []
            for idle in [20, 200]:
                path = MODEL["create_catalog"](Path(temporary) / (str(idle) + ".sqlite"), idle_collections=idle)
                with MODEL["Catalog"](path) as catalog:
                    selection = catalog.select("synthetic", "hot")
                    measurements.append((len(selection.routes), catalog.metadata_statements, catalog.returned_rows))
                    self.assertTrue(all("SEARCH" in str(row) and "PRIMARY KEY" in str(row) for row in catalog.selection_query_plan()))
                    self.assertEqual(selection.routes[0].lower, 0)
                    self.assertEqual(selection.routes[-1].upper, MODEL["HASH_SPACE"])
            self.assertEqual(measurements, [(16, 2, 17), (16, 2, 17)])

    def test_keyset_pages_cover_original_catalog_once_without_cross_tenant_results(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = MODEL["create_catalog"](Path(temporary) / "catalog.sqlite", idle_collections=20)
            with MODEL["Catalog"](path) as catalog:
                names, after = [], ""
                while True:
                    page, next_key = catalog.page("synthetic", after=after, limit=3)
                    names.extend(row[0] for row in page)
                    if next_key is None: break
                    after = next_key
                self.assertEqual(names, sorted(["hot", *["idle-" + str(index).zfill(8) for index in range(20)]]))
                self.assertEqual(catalog.page("other", limit=3), ([], None))

    def test_total_selection_limit_plus_one_refuses_without_partial_result(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = MODEL["create_catalog"](Path(temporary) / "catalog.sqlite", idle_collections=0, selected_routes=65)
            with MODEL["Catalog"](path) as catalog:
                with self.assertRaisesRegex(MODEL["Refused"], "selected_partitions"):
                    catalog.select("synthetic", "hot", max_selected=64)
                self.assertEqual(catalog.metadata_statements, 2)
                self.assertEqual(len(catalog.select("synthetic", "hot", lower=0, upper=1).routes), 1)

    def test_unknown_types_keys_and_existing_outputs_refuse(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = MODEL["create_catalog"](Path(temporary) / "catalog.sqlite", idle_collections=0)
            original = path.read_bytes()
            with self.assertRaises(FileExistsError):
                MODEL["create_catalog"](path, idle_collections=0)
            self.assertEqual(path.read_bytes(), original)
            with MODEL["Catalog"](path) as catalog:
                for kwargs in [{"limit": True}, {"limit": 1001}, {"limit": 0}]:
                    with self.assertRaises(MODEL["Refused"]): catalog.page("synthetic", **kwargs)
                for cursor in [False, 0, '\ud800']:
                    with self.assertRaises(MODEL["Refused"]): catalog.page("synthetic", after=cursor)
                with self.assertRaises(MODEL["Refused"]): catalog.select("synthetic", "missing")
                with self.assertRaises(MODEL["Refused"]): catalog.select("synthetic", "hot", upper=True)
