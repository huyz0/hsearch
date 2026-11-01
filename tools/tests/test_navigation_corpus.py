"""Corpus failures must remain visible without discarding unexpected work."""
from contextlib import redirect_stdout
import io
import os
from pathlib import Path
import runpy
from types import SimpleNamespace
import tempfile
import unittest

API = runpy.run_path(str(Path(__file__).resolve().parents[1] / "navigation_corpus.py"))
Refused = API["API"]["Refused"]


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
