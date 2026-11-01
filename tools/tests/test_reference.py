"""Read-only evidence binding cannot confer task authority or hide source edits."""
from pathlib import Path
import runpy
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
TASK = runpy.run_path(str(ROOT / "tools/task.py"))


@unittest.skipUnless(shutil.which("git"), "Real source binding requires Git")
class ReadOnlySourceControls(unittest.TestCase):
    def test_detached_evidence_remains_clean_but_cannot_acquire_task_authority(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder) / "source"
            root.mkdir()
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.STDOUT, text=True, timeout=10).strip()
            git("init", "-q", "-b", "main")
            git("config", "user.name", "Readonly fixture")
            git("config", "user.email", "fixture@example.invalid")
            git("config", "core.autocrlf", "false")
            (root / "tracked.txt").write_bytes(b"committed\n")
            git("add", ".")
            git("commit", "-qm", "fixture")
            head = git("rev-parse", "HEAD")
            git("checkout", "-q", "--detach", head)
            result = TASK["source_fingerprint"](root, head, [])
            self.assertTrue(result["clean"])
            self.assertEqual(result["head"], head)
            store = TASK["TaskStore"](Path(folder) / "state", root)
            record = {"worktree": str(root), "branch": "main", "packet": {"base_revision": head, "owned_paths": []}}
            with self.assertRaisesRegex(TASK["Refused"], "registration/branch changed"):
                store.fingerprint(record)
            git("update-index", "--skip-worktree", "tracked.txt")
            (root / "tracked.txt").write_bytes(b"hidden changed bytes\n")
            with self.assertRaisesRegex(TASK["Refused"], "Hidden index"):
                TASK["source_fingerprint"](root, head, [])


if __name__ == "__main__":
    unittest.main()
