import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "docs/plan/tools"))
import authority_model as am  # noqa: E402

CKPT = dict(checkpoint=True, reads=True, instances=2, slots=4, records=3)


class ExploreTests(unittest.TestCase):
    def test_finds_a_shortest_counterexample(self):
        # 0 -> 1 -> 2 -> 3 and a shortcut 0 -> 3 labelled "jump": the shortest path is the jump.
        graph = {0: [("a", 1), ("jump", 3)], 1: [("b", 2)], 2: [("c", 3)], 3: []}
        r = am.explore(0, lambda s: graph[s], lambda s: "bad" if s == 3 else None)
        self.assertFalse(r["safe"])
        self.assertEqual(r["counterexample"], ["jump"])

    def test_an_unfinished_search_is_never_reported_safe(self):
        with self.assertRaises(RuntimeError):
            am.explore(0, lambda s: [("n", s + 1)], lambda s: None, limit=50)

    def test_an_initial_violation_is_reported(self):
        self.assertFalse(am.explore(0, lambda s: [], lambda s: "bad")["safe"])


class LaneTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.base = am.lane_authority()
        cls.ckpt = am.lane_authority(**CKPT)

    def test_the_correct_protocol_is_safe_and_the_search_is_not_trivial(self):
        self.assertTrue(self.base["safe"])
        self.assertGreater(self.base["states"], 10_000)
        self.assertTrue(self.ckpt["safe"])
        self.assertGreater(self.ckpt["states"], 100_000)

    def test_every_authority_control_is_refused_with_its_reason(self):
        expect = {
            "BUG_two_appends_in_flight": "not recovered",
            "BUG_continue_after_failure": "not recovered",
            "BUG_seal_recorded_without_a_fence": "not recovered",
            "BUG_serve_before_establishing": "not recovered",
            "BUG_blind_head_write": "not recovered",
        }
        self.assertEqual(set(expect), set(am.LANE_CONTROLS))
        for name, reason in expect.items():
            r = am.lane_authority(**am.LANE_CONTROLS[name])
            self.assertFalse(r["safe"], name)
            self.assertIn(reason, r["violation"], name)

    def test_every_checkpoint_and_read_control_is_refused(self):
        controls = {**am.CHECKPOINT_CONTROLS, **am.READ_CONTROLS}
        want = {"BUG_head_before_pages": "page that does not exist", "BUG_cached_head_read_as_latest": "read missed"}
        for name, kwargs in controls.items():
            r = am.lane_authority(**CKPT, **kwargs)
            self.assertFalse(r["safe"], name)
            self.assertIn(want.get(name, "acknowledged record"), r["violation"], name)

    def test_a_late_put_into_a_deleted_fence_is_the_counterexample(self):
        r = am.lane_authority(**CKPT, reclaim_fences=True)
        self.assertIn("PUT of record 0 to lane 0 slot 1 succeeds, reply delivered", r["counterexample"][-1])
        self.assertTrue(any("reclaim lane 0 slot 1 ('F')" in step for step in r["counterexample"]))

    def test_sealing_must_look_for_the_tail_at_or_above_the_cover(self):
        r = am.lane_authority(**CKPT, seal_from_cover=False)
        self.assertFalse(r["safe"])
        self.assertTrue(any("reclaim lane 0 slot 1" in step for step in r["counterexample"]))
        self.assertTrue(any("seals lane 0 at slot 1" in step for step in r["counterexample"]))

    def test_recovery_reads_pages_and_lanes_from_the_cover(self):
        slots = 4
        lanes = (("E", ("R", 0), ("R", 1), None),)
        head = (3, (0,), (-1,), (2,), (("k", 0),))
        got, error = am.lane_recover(head, lanes, frozenset({(("k", 0), (9,))}), slots)
        self.assertIsNone(error)
        self.assertEqual(got, {9, 1})  # slot 1 is below the cover, covered by the page
        _, error = am.lane_recover(head, lanes, frozenset(), slots)
        self.assertIn("does not exist", error)

    def test_a_sealed_lane_is_read_only_to_its_tail_locator(self):
        lanes = (("E", ("R", 0), "F", ("R", 7)),)
        head = (2, (0, 1), (2, -1), (0, 0, 0), ())
        lanes = lanes + ((None,) * 4,)
        got, _ = am.lane_recover(head, lanes, frozenset(), 4)
        self.assertEqual(got, {0})  # the record after the fence is never read

    def test_a_stale_head_cache_is_only_unsafe_as_latest(self):
        r = am.lane_authority(**CKPT, cached_read=True)
        self.assertIn("read missed", r["violation"])


class PinGcTests(unittest.TestCase):
    def test_the_protocol_is_safe_and_each_control_is_refused(self):
        self.assertTrue(am.pin_gc()["safe"])
        self.assertEqual(len(am.PIN_CONTROLS), 4)
        for name, kwargs in am.PIN_CONTROLS.items():
            r = am.pin_gc(**kwargs)
            self.assertFalse(r["safe"], name)
            self.assertIn("lost its objects" if name != "BUG_blind_publication" else "deleted", r["violation"], name)

    def test_a_pin_after_the_boundary_is_abandoned_not_honoured(self):
        r = am.pin_gc(generations=2, readers=1, compactors=1)
        self.assertTrue(r["safe"])


class PromotionTests(unittest.TestCase):
    def test_membership_ceiling_and_forgetting(self):
        self.assertTrue(am.membership()["safe"])
        r = am.membership(versioned=False)
        self.assertIn("forgotten", r["violation"])
        r = am.membership(check_cap=False)
        self.assertIn("above its ceiling", r["violation"])

    def test_promotion_is_safe_with_unknown_outcomes_and_racing_promoters(self):
        r = am.promotion()
        self.assertTrue(r["safe"])
        self.assertGreater(r["states"], 1000)
        self.assertTrue(am.promotion(promoters=1)["safe"])

    def test_every_promotion_control_is_refused(self):
        self.assertEqual(set(am.PROMOTION_CONTROLS), {"BUG_import_before_the_seal", "BUG_descriptor_grants_write_authority", "BUG_rollback_over_a_seal_after_an_unknown_outcome"})
        for name, kwargs in am.PROMOTION_CONTROLS.items():
            self.assertFalse(am.promotion(**kwargs)["safe"], name)


class KeyTests(unittest.TestCase):
    def test_the_m03_09_histories_agree(self):
        ref, ref_fail = am.lane_mate_histories_agree()
        self.assertTrue(ref["per_index_dek"]["safe"] and ref["lane_dek_default"]["safe"])
        self.assertFalse(ref["lane_dek_a_byok"]["safe"])
        self.assertFalse(ref_fail["safe"])
        for name, expect in (("per_index_dek, skip, authenticated index and position", True), ("lane_dek_default, skip", True),
                             ("BUG_revocable_key_wraps_a_shared_lane", False), ("BUG_recovery_fails_the_lane", False)):
            self.assertEqual(am.key_histories(**am.KEY_CASES[name])["safe"], expect, name)

    def test_the_model_finds_the_same_lane_mate_counterexample(self):
        r = am.key_histories(**am.KEY_CASES["BUG_revocable_key_wraps_a_shared_lane"])
        self.assertIn("revoke_a", r["counterexample"])
        self.assertIn("append_b", r["counterexample"])

    def test_each_new_history_family_has_a_refused_control_and_a_passing_rule(self):
        want = {"BUG_no_aad": "applied as index", "BUG_aad_without_the_index": "applied as index", "BUG_aad_without_the_position": "twice",
                "BUG_recovery_drops_revoked_blocks": "not revoked", "BUG_revoke_only_the_current_key_version": "is revoked"}
        for name, reason in want.items():
            r = am.key_histories(**am.KEY_CASES[name])
            self.assertFalse(r["safe"], name)
            self.assertIn(reason, r["why"], name)
        safe = am.key_histories(**am.KEY_CASES["per_index_dek, skip, authenticated index and position"])
        self.assertTrue(safe["safe"])
        self.assertGreater(safe["histories"], 50_000)

    def test_a_reinstated_key_finds_its_blocks_only_if_recovery_kept_them(self):
        history = ("append_a", "revoke_a", "recover", "reinstate_a", "recover")
        # Re-run the exhaustive search at that exact length: skip passes, drop fails.
        self.assertTrue(am.key_histories("per_index_dek", length=5)["safe"])
        self.assertFalse(am.key_histories("per_index_dek", recovery="drop", length=5)["safe"])
        self.assertEqual(len(history), 5)


class ReportTests(unittest.TestCase):
    def test_selftest_and_markdown(self):
        result = am.run_all()
        for model, cases in result["models"].items():
            for name, r in cases.items():
                self.assertTrue(am.verdict_ok(name, r), (model, name))
        text = am.markdown(result)
        self.assertIn("**unsafe** (expected)", text)
        for line in text.splitlines():
            if line.startswith("| BUG"):
                self.assertIn("**unsafe** (expected)", line)


if __name__ == "__main__":
    unittest.main()
