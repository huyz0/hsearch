"""Corpus failures must remain visible without discarding unexpected work."""
from contextlib import redirect_stdout
import io
import hashlib
import json
import os
from pathlib import Path
import runpy
from types import SimpleNamespace
import tempfile
import unittest

API = runpy.run_path(str(Path(__file__).resolve().parents[1] / "navigation_corpus.py"))
Refused = API["API"]["Refused"]


class WitnessControls(unittest.TestCase):
    def response(self, items=None):
        return {"status": "semantic", "semantic": True,
                "source": {"head": "a" * 40, "digest": "b" * 64},
                "tool_identity": {"lsp_revision": "c" * 40, "lsp_sha256": "d" * 64},
                "data": {"items": items if items is not None else [
                    {"severity": "error", "code": "E0308", "line": 2}]}}

    def observe(self, response, acknowledged=None):
        lines = []
        witness = API["ColdWitness"](lines.append)
        self.assertTrue(witness.emit(response, 0, acknowledged))
        self.assertEqual(len(lines), 1)
        return witness, json.loads(lines[0])

    def test_actual_diagnostic_and_current_identity_are_retained(self):
        witness, record = self.observe(self.response(), {"lsp_sha256": "e" * 64})
        self.assertEqual(record["items"], [{"item_index": 0, "severity": "error", "code": "E0308", "line": 2}])
        self.assertTrue(record["e0308_line2_present"])
        self.assertEqual(record["source_digest"], "b" * 64)
        self.assertEqual(record["lsp_sha256"], "d" * 64)
        self.assertEqual(record["identity_origin"], "context_response")
        self.assertEqual(witness.records, 1)
        self.assertTrue(witness.complete)

    def test_expected_metadata_cannot_turn_wrong_or_empty_items_into_match(self):
        for items in ([], [{"severity": "error", "code": "E0308", "line": 3}],
                      [{"severity": "error", "code": "E9999", "line": 2}]):
            response = self.response(items)
            response["expected"] = {"code": "E0308", "line": 2}
            _, record = self.observe(response)
            self.assertFalse(record["e0308_line2_present"])

    def test_actual_target_after_prefix_keeps_its_original_index(self):
        items = [{"severity": "warning", "code": "W1", "line": 4}] * 9
        items += [{"severity": "error", "code": "E0308", "line": 2}]
        _, record = self.observe(self.response(items))
        self.assertEqual(record["diagnostics_count"], 10)
        self.assertEqual(record["items"][0]["item_index"], 9)
        self.assertEqual(record["items"][0]["code"], "E0308")
        self.assertLessEqual(len(record["items"]), 8)
        self.assertTrue(record["truncated"])

    def test_failed_context_uses_labelled_acknowledged_identity(self):
        response = self.response()
        response.pop("tool_identity")
        response.update(status="fallback", semantic=False, reason="navigation_cli_query_failed",
                        data={"semantic_failure": {"exit_code": 1, "stderr_head": "LSP error -32800"}})
        _, record = self.observe(response, {"lsp_revision": "e" * 40, "lsp_sha256": "f" * 64})
        self.assertFalse(record["semantic"])
        self.assertIsNone(record["diagnostics_count"])
        self.assertEqual(record["diagnostics_shape"], "unavailable")
        self.assertEqual(record["identity_origin"], "acknowledged_start")
        self.assertEqual(record["lsp_sha256"], "f" * 64)
        self.assertEqual(record["reason"], "navigation_cli_query_failed")

    def test_missing_identity_and_invalid_digests_are_not_invented(self):
        response = self.response()
        response.pop("tool_identity")
        response["source"] = {"digest": "not-a-digest"}
        _, record = self.observe(response)
        self.assertEqual(record["identity_origin"], "unavailable")
        self.assertIsNone(record["lsp_sha256"])
        self.assertIsNone(record["source_digest"])
        self.assertIsNone(record["source_head"])

    def test_serialized_control_characters_and_unicode_respect_record_bound(self):
        response = self.response([{"severity": "\x00" * 32, "code": "\x01\u03bb" * 100, "line": 4}] * 8)
        response["reason"] = "\x02\u03bb" * 300
        witness, record = self.observe(response)
        self.assertLessEqual(len(API["encoded"](record)) + 1, witness.RECORD_BYTES)
        self.assertTrue(record["truncated"])
        self.assertEqual(witness.bytes, len(API["encoded"](record)) + 1)

    def test_whole_run_budget_refuses_before_writing(self):
        lines = []
        witness = API["ColdWitness"](lines.append)
        witness.bytes = witness.TOTAL_BYTES - 1
        self.assertFalse(witness.emit(self.response(), 0))
        self.assertEqual(lines, [])
        self.assertFalse(witness.complete)

    def test_closed_sink_cannot_replace_original_failure(self):
        def closed(_value):
            raise BrokenPipeError("closed")
        witness = API["ColdWitness"](closed)
        primary = Refused("actual semantic oracle failure")
        with self.assertRaises(Refused) as caught:
            try:
                raise primary
            finally:
                self.assertFalse(witness.emit(self.response(), 0))
        self.assertIs(caught.exception, primary)
        self.assertFalse(witness.complete)
        self.assertEqual(witness.records, 0)
        self.assertGreater(witness.bytes, 0)

    def test_full_bounded_response_is_hashed_and_changes_with_source(self):
        response = self.response()
        _, first = self.observe(response)
        self.assertEqual(first["response_sha256"], hashlib.sha256(API["encoded"](response)).hexdigest())
        response["source"]["digest"] = "e" * 64
        _, second = self.observe(response)
        self.assertNotEqual(first["response_sha256"], second["response_sha256"])

    def test_oversized_context_response_cannot_qualify_evidence(self):
        response = self.response()
        response["extra"] = "x" * 131072
        lines = []
        witness = API["ColdWitness"](lines.append)
        self.assertFalse(witness.emit(response, 0))
        self.assertFalse(witness.complete)
        self.assertEqual(lines, [])


class CleanupControls(unittest.TestCase):
    def test_closed_logging_sink_cannot_replace_primary_error(self):
        def fails(*_args):
            raise Refused("cleanup failed")
        class ClosedSink:
            def write(self, _text):
                raise BrokenPipeError("closed diagnostic stream")
        primary = Refused("original diagnostic failure")
        store = SimpleNamespace(get=lambda _claim: {"phase": "allocated"}, cleanup=fails)
        with self.assertRaises(Refused) as caught, redirect_stdout(ClosedSink()):
            try:
                raise primary
            finally:
                API["cleanup_claims"](store, [("claim", "owner")], None, lambda *_args: {"status": "stopped"}, {}, API["sys"].exception())
        self.assertIs(caught.exception, primary)

    @unittest.skipUnless(os.name == "posix", "Native link preservation is qualified on Linux")
    def test_owned_cleanup_preserves_hardlinked_fixture_without_closing_claim(self):
        with tempfile.TemporaryDirectory() as folder:
            path, alias = Path(folder) / "fixture.rs", Path(folder) / "alias.rs"
            path.write_bytes(b"known edit")
            os.link(path, alias)
            calls = []
            store = SimpleNamespace(get=lambda _claim: {"phase": "allocated", "worktree": folder}, cleanup=lambda *_args: calls.append("closed"))
            with self.assertRaisesRegex(Refused, "cleanup incomplete"), redirect_stdout(io.StringIO()):
                API["cleanup_claims"](store, [("claim", "owner")], None, lambda *_args: {"status": "stopped"},
                                      {"claim": [(path, b"original", b"known edit")]}, None)
            self.assertEqual(path.read_bytes(), b"known edit")
            self.assertEqual(alias.read_bytes(), b"known edit")
            self.assertEqual(calls, [])

    def test_primary_failure_is_not_masked_by_cleanup_failure(self):
        def fails(*_args):
            raise Refused("cleanup failed")
        store = SimpleNamespace(get=lambda _claim: {"phase": "allocated"}, cleanup=fails)
        output = io.StringIO()
        with self.assertRaisesRegex(Refused, "original diagnostic failure"), redirect_stdout(output):
            try:
                raise Refused("original diagnostic failure")
            finally:
                API["cleanup_claims"](store, [("claim", "owner")], None, lambda *_args: {"status": "stopped"}, {}, API["sys"].exception())
        self.assertIn("cleanup failed", output.getvalue())

    def test_cleanup_failure_without_a_primary_error_cannot_pass(self):
        def fails(*_args):
            raise Refused("cleanup failed")
        store = SimpleNamespace(get=lambda _claim: {"phase": "allocated"}, cleanup=fails)
        with self.assertRaisesRegex(Refused, "cleanup incomplete"), redirect_stdout(io.StringIO()):
            API["cleanup_claims"](store, [("claim", "owner")], None, lambda *_args: {"status": "stopped"}, {}, None)

    def test_only_exact_owned_fixture_edits_are_restored(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "fixture.rs"
            path.write_bytes(b"known edit")
            calls = []
            store = SimpleNamespace(get=lambda _claim: {"phase": "allocated", "worktree": folder}, cleanup=lambda *_args: calls.append("closed"))
            edits = {"claim": [(path, b"original", b"known edit")]}
            API["cleanup_claims"](store, [("claim", "owner")], None, lambda *_args: {"status": "stopped"}, edits, None)
            self.assertEqual(path.read_bytes(), b"original")
            self.assertEqual(calls, ["closed"])
            path.write_bytes(b"user work")
            with self.assertRaisesRegex(Refused, "cleanup incomplete"), redirect_stdout(io.StringIO()):
                API["cleanup_claims"](store, [("claim", "owner")], None, lambda *_args: {"status": "stopped"}, edits, None)
            self.assertEqual(path.read_bytes(), b"user work")
            self.assertEqual(calls, ["closed"])


if __name__ == "__main__":
    unittest.main()
