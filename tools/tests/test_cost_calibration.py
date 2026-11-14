import copy
import json
import sys
import unittest
from decimal import Decimal
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "docs/plan/tools"))
import cost_calibration as cc  # noqa: E402

DATA = json.loads(cc.DEFAULT.read_text(encoding="utf-8"))


class CostCalibrationTests(unittest.TestCase):
    def test_rate_card_requires_provenance_for_every_price(self):
        cc.check_rate_card(DATA["rate_card"])
        for mutate in (lambda c: c["requests"]["put"].pop("source"),
                       lambda c: c["requests"]["get"].update(as_of="soon"),
                       lambda c: c["requests"]["put"].update(usd_per_1000="-1"),
                       lambda c: c["traffic"]["cross_az"].pop("unit"),
                       lambda c: c.pop("region")):
            broken = copy.deepcopy(DATA["rate_card"])
            mutate(broken)
            with self.assertRaises(ValueError):
                cc.check_rate_card(broken)

    def test_unpriced_path_is_rejected_not_priced_at_zero(self):
        with self.assertRaises(ValueError):
            cc.price(DATA["rate_card"], {"unpriced": 1.0}, 0)

    def test_idle_costs_nothing(self):
        self.assertEqual(cc.zero_idle(DATA["rate_card"]), Decimal(0))

    def test_poisson_window_model_matches_the_simulation(self):
        result = cc.run(DATA)
        for row in result["checks"]["poisson_vs_simulation"]:
            self.assertLess(row["relative_difference"], 0.05, row)

    def test_profiles_price_what_they_claim(self):
        rows = {(r["workload"], r["profile"]): r for r in cc.run(DATA)["rows"]}
        for wl in ("sparse", "busy", "skewed"):
            # Only the three-zone journal pays cross-AZ bytes; regional object profiles pay none.
            self.assertGreater(rows[(wl, "latency-ha")]["cross_az_bytes_per_s"], 0)
            for p in ("object-economy", "object-resilient", "latency-zonal"):
                self.assertEqual(rows[(wl, p)]["cross_az_bytes_per_s"], 0)
            # A longer batching wait can only reduce object requests.
            self.assertLessEqual(rows[(wl, "object-economy")]["requests_per_s"]["put"],
                                 rows[(wl, "object-resilient")]["requests_per_s"]["put"])
        # Three full voters add cost over one zone's journal, all else equal.
        self.assertGreater(Decimal(rows[("busy", "latency-ha")]["usd_per_30_days_subtotal"]),
                           Decimal(rows[("busy", "latency-zonal")]["usd_per_30_days_subtotal"]))

    def test_recovery_charges_the_cold_list_and_scales_with_tail(self):
        spec = DATA["workloads"]["recovery"]
        counts, usd, tail = cc.recovery(spec, DATA["rate_card"])
        self.assertEqual(counts["list"], spec["active_domains"] * spec["cold_list_per_domain"])
        bigger = dict(spec, tail_batches_per_domain=spec["tail_batches_per_domain"] * 2)
        counts2, usd2, tail2 = cc.recovery(bigger, DATA["rate_card"])
        self.assertGreater(usd2, usd)
        self.assertEqual(tail2, tail * 2)

    def test_price_change_is_a_data_update(self):
        base = cc.run(DATA)["rows"][0]["usd_per_30_days_subtotal"]
        cheaper = copy.deepcopy(DATA)
        cheaper["rate_card"]["requests"]["put"]["usd_per_1000"] = "0.0025"
        changed = cc.run(cheaper)["rows"][0]["usd_per_30_days_subtotal"]
        self.assertLess(Decimal(changed), Decimal(base))


if __name__ == "__main__":
    unittest.main()
