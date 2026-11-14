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
CARD = DATA["rate_card"]
CKPT = DATA["checkpoint"]
ECONOMY, HA = DATA["profiles"]["object-economy"], DATA["profiles"]["latency-ha"]
# Two records in one domain 0.1 s apart (one batch at a 0.25 s wait), one in another domain.
EVENTS = [(0.0, 0, 1000), (0.1, 2, 1000), (0.2, 1, 1000)]


class CostCalibrationTests(unittest.TestCase):
    def test_rate_card_requires_provenance_and_a_known_status(self):
        cc.check_rate_card(CARD)
        for mutate in (lambda c: c["requests"]["put"].pop("source"),
                       lambda c: c["requests"]["get"].update(as_of="soon"),
                       lambda c: c["requests"]["put"].update(usd_per_1000="-1"),
                       lambda c: c["traffic"]["cross_az"].pop("unit"),
                       lambda c: c.pop("region"),
                       lambda c: c.update(status="quote")):
            broken = copy.deepcopy(CARD)
            mutate(broken)
            with self.assertRaises(ValueError):
                cc.check_rate_card(broken)

    def test_unpriced_path_is_rejected_not_priced_at_zero(self):
        with self.assertRaises(ValueError):
            cc.price(CARD, {"unpriced": 1.0}, 0)

    def test_an_empty_trace_prices_to_zero_through_every_profile(self):
        self.assertEqual(cc.idle_cost(CARD, DATA), Decimal(0))

    def test_exact_counts_for_a_hand_built_trace(self):
        # Domains (0,0) and (0,1): two batches in 1 s. Object profile: 2 PUT x (1 + 2/64), 2 KMS.
        counts, cross, _ = cc.profile_counts(EVENTS, 1, dict(ECONOMY, max_wait_s=0.25), CKPT)
        self.assertAlmostEqual(counts["kms"], 2.0)
        self.assertAlmostEqual(counts["put"], 2 * (1 + 2 / 64))
        self.assertEqual(cross, 0)
        # Journal profile: KMS unchanged, object PUTs are only the archive batches (2 domains, one
        # window each at 5 s), and cross-AZ bytes are 3000 raw bytes x 2 copies.
        counts, cross, _ = cc.profile_counts(EVENTS, 1, HA, CKPT)
        self.assertAlmostEqual(counts["kms"], 2.0)
        self.assertAlmostEqual(counts["put"], 2 * (1 + 2 / 64))
        self.assertEqual(cross, 3000 * 2)

    def test_cross_az_cost_is_exactly_bytes_times_the_combined_rate(self):
        bytes_s = 1 << 30
        components, _ = cc.price(CARD, {}, bytes_s)
        seconds = Decimal(CARD["period_seconds"])
        self.assertEqual(Decimal(components["traffic:cross_az"]), (seconds * Decimal("0.02")).quantize(Decimal("0.01")))

    def test_journal_profiles_do_not_charge_commits_as_object_puts(self):
        events = cc.workload_trace("busy", DATA["workloads"]["busy"], 7)
        obj, _, _ = cc.profile_counts(events, 30, DATA["profiles"]["object-resilient"], CKPT)
        ha, _, _ = cc.profile_counts(events, 30, HA, CKPT)
        self.assertLess(ha["put"], obj["put"] / 5)
        self.assertAlmostEqual(ha["kms"], obj["kms"])

    def test_workload_inputs_drive_the_trace(self):
        base = DATA["workloads"]["busy"]
        small = cc.workload_trace("busy", dict(base, records_per_s=base["records_per_s"] // 2), 7)
        big = cc.workload_trace("busy", base, 7)
        self.assertLess(len(small), len(big) * 0.6)
        self.assertEqual({e[2] for e in cc.workload_trace("busy", dict(base, record_bytes=77), 7)}, {77})
        hot = cc.workload_trace("busy", dict(base, hot_share=0.9), 7)
        self.assertGreater(sum(1 for e in hot if e[1] < 50) / len(hot), 0.85)

    def test_poisson_formula_holds_for_poisson_and_overstates_bursty_traffic(self):
        rows = {(r["workload"], r["wait_s"]): r for r in cc.run(DATA)["checks"]["poisson_vs_simulation"]}
        for key in (("sparse", 0.25), ("busy", 0.25), ("busy", 1.0)):
            self.assertLess(rows[key]["relative_difference"], 0.03, rows[key])
        for key in (("bursty", 0.25), ("bursty", 1.0)):
            self.assertGreater(rows[key]["relative_difference"], 0.5, rows[key])
            self.assertGreater(rows[key]["poisson_model_batches_per_s"], rows[key]["simulated_batches_per_s"])

    def test_recovery_counts_prices_and_scales_with_the_tail(self):
        spec = DATA["workloads"]["recovery"]
        counts, usd, tail = cc.recovery(spec, CARD)
        d = spec["active_domains"]
        self.assertEqual(counts["get"], d * 66)
        self.assertEqual(counts["list"], d)
        self.assertEqual(counts["put"], d * spec["reactivation_puts_per_domain"])
        expected = (Decimal("0.0004") * 66 * d + Decimal("0.005") * d + Decimal("0.005") * 2 * d + Decimal("0.003") * d) / 1000
        self.assertEqual(usd, expected)
        _, usd2, tail2 = cc.recovery(dict(spec, tail_batches_per_domain=128), CARD)
        self.assertEqual(tail2, tail * 2)
        self.assertGreater(usd2, usd)
        self.assertEqual(cc.tail_read_seconds(120 * 2**20, 120), 1)

    def test_price_change_is_a_data_update_for_every_row(self):
        base = cc.run(DATA)["rows"]
        cheaper = copy.deepcopy(DATA)
        cheaper["rate_card"]["requests"]["put"]["usd_per_1000"] = "0.0025"
        changed = cc.run(cheaper)["rows"]
        for before, after in zip(base, changed):
            self.assertLess(Decimal(after["usd_per_30_days_subtotal"]), Decimal(before["usd_per_30_days_subtotal"]))


if __name__ == "__main__":
    unittest.main()
