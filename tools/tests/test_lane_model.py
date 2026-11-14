import json
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "docs/plan/tools"))
import cost_calibration as cc  # noqa: E402
import lane_model as lm  # noqa: E402

DATA = json.loads(cc.DEFAULT.read_text(encoding="utf-8"))
CFG = DATA["lane_sharing"]
CKPT = DATA["checkpoint"]
A, B, C = lm.OPTIONS


class LaneModelTests(unittest.TestCase):
    def test_idle_indexes_issue_no_requests_in_any_option(self):
        for option in lm.OPTIONS:
            r = lm.run_option(option, [], CFG, 0.25, 30, CKPT)
            self.assertEqual((r["put_per_s"], r["kms_per_s"], r["lanes_active"]), (0, 0, 0))

    def test_exact_counts_for_a_hand_built_storm(self):
        # Four writes to four different indexes, none BYOK-flagged.
        cfg = dict(CFG, byok_fraction=0.0, lane_group_size=1000)
        events = [(0.0, 1, 1000), (0.01, 2, 1000), (0.02, 3, 1000), (0.03, 4, 1000)]
        shared = lm.run_option(B, events, cfg, 0.25, 1, CKPT)
        self.assertAlmostEqual(shared["batches_per_s"], 1.0)      # one lane, one batch
        self.assertAlmostEqual(shared["put_per_s"], 1 + 2 / 64)
        own = lm.run_option(A, events, cfg, 0.25, 1, CKPT)
        self.assertAlmostEqual(own["batches_per_s"], 4.0)         # a lane, and a batch, per index
        self.assertAlmostEqual(own["put_per_s"], 4 * (1 + 2 / 64))

    def test_byok_indexes_never_share_a_lane_under_option_b(self):
        cfg = dict(CFG, byok_fraction=1.0, lane_group_size=1000)
        events = [(0.0, 1, 1000), (0.01, 2, 1000)]
        self.assertEqual(lm.run_option(B, events, cfg, 0.25, 1, CKPT)["lanes_active"], 2)
        self.assertEqual(lm.run_option(C, events, cfg, 0.25, 1, CKPT)["lanes_active"], 1)

    def test_per_index_encryption_pays_a_wrap_per_byok_record(self):
        cfg = dict(CFG, byok_fraction=1.0, lane_group_size=1000)
        events = [(0.0, 1, 1000), (0.01, 2, 1000), (0.02, 3, 1000)]
        r = lm.run_option(C, events, cfg, 0.25, 1, CKPT)
        self.assertAlmostEqual(r["kms_per_s"], 1 + 3)  # one lane wrap plus three index wraps

    def test_sharing_beats_a_lane_per_index_on_requests_and_idle_objects(self):
        rows = {(r["workload"], r["option"]): r for r in lm.compare(DATA)["rows"]}
        for w in ("steady_small", "write_storm"):
            a, b, c = rows[(w, A)], rows[(w, B)], rows[(w, C)]
            self.assertGreater(a["requests_per_ack"], 3 * b["requests_per_ack"])
            self.assertGreater(a["retained_objects_total"], 10 * b["retained_objects_total"])
            self.assertGreater(b["retained_objects_total"], c["retained_objects_total"])
            self.assertEqual((a["blast_radius_indexes"], b["blast_radius_indexes"]), (1, CFG["lane_group_size"]))

    def test_option_b_degrades_toward_a_lane_per_index_as_byok_grows(self):
        sens = {r["byok_fraction"]: r for r in lm.byok_sensitivity(DATA)}
        self.assertGreater(sens[1.0]["B"], 3 * sens[0.05]["B"])
        self.assertLess(sens[1.0]["C"], 0.8 * sens[1.0]["B"])

    def test_revocation_model(self):
        self.assertTrue(lm.revocation_search("per_index_dek")["safe"])
        self.assertTrue(lm.revocation_search("lane_dek_default")["safe"])
        broken = lm.revocation_search("lane_dek_a_byok")
        self.assertFalse(broken["safe"])
        self.assertIn("revoke_key_a", broken["counterexample"])
        self.assertEqual(broken["record"], "b")  # the lane-mate is the casualty

    def test_hottest_share_ignores_cold_lanes_with_a_lucky_double_hit(self):
        cold = {("grp", 0): lm.Counter({1: 2, 2: 1, 3: 1, 4: 1, 5: 15})}   # 20 records, 75% on one index
        self.assertEqual(lm.hottest_share(cold, 1000), 0.0)
        busy = {("grp", 1): lm.Counter({1: 1500, 2: 500})}
        self.assertAlmostEqual(lm.hottest_share(busy, 1000), 0.75)
        own = {("idx", 7): lm.Counter({7: 5000})}                           # a lane per index is not a share
        self.assertEqual(lm.hottest_share(own, 1000), 0.0)

    def test_promotion_needs_both_a_share_and_a_volume_floor(self):
        sparse = {("grp", 0): lm.Counter({9: 3})}                          # 100% share of 3 records
        self.assertEqual(lm.promotion_candidates(sparse, CFG), [])
        hot = {("grp", 0): lm.Counter({9: 900, 1: 100, 2: 100})}           # 82% of 1,100
        self.assertEqual(lm.promotion_candidates(hot, CFG), [(("grp", 0), 9)])
        even = {("grp", 0): lm.Counter({i: 10 for i in range(200)})}       # busy, nobody above a quarter
        self.assertEqual(lm.promotion_candidates(even, CFG), [])

    def test_skewed_load_does_not_promote_but_one_hot_index_does(self):
        rows = {(r["workload"], r["option"]): r for r in lm.compare(DATA)["rows"]}
        self.assertEqual(rows[("steady_small", B)]["promoted_indexes"], 0)
        self.assertLess(rows[("steady_small", B)]["max_index_share_of_busy_lane"], 0.01)
        self.assertGreaterEqual(rows[("one_hot_index", B)]["promoted_indexes"], 1)
        self.assertGreater(rows[("one_hot_index", B)]["max_index_share_of_busy_lane"], 0.9)

    def test_recovery_that_fails_the_lane_on_a_revoked_block_breaks_lane_mates(self):
        failing = lm.revocation_search("per_index_dek", recovery="fail")
        self.assertFalse(failing["safe"])
        self.assertIn("recovery failed", failing["why"])
        self.assertTrue(lm.revocation_search("per_index_dek", recovery="skip")["safe"])


if __name__ == "__main__":
    unittest.main()
