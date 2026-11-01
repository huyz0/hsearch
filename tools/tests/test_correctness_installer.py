"""Pinned archive integrity and preservation controls; no network or tool execution."""
from pathlib import Path
import gzip
import hashlib
import io
import json
import runpy
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
INSTALL = runpy.run_path(str(ROOT / "dev/install_correctness.py"))


def package(members):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as archive:
        for name, kind, payload in members:
            item = tarfile.TarInfo(name)
            item.type = kind
            item.size = len(payload) if kind == tarfile.REGTYPE else 0
            archive.addfile(item, io.BytesIO(payload) if item.size else None)
    return gzip.compress(raw.getvalue())


class PinnedToolControls(unittest.TestCase):
    def fixture(self, archive):
        profile = json.loads((ROOT / "dev/correctness-tools.json").read_text())
        pin = profile["nextest"]["architectures"]["x86_64"]
        pin.update(archive_bytes=len(archive), archive_sha256=hashlib.sha256(archive).hexdigest())
        return profile

    def test_traversal_links_duplicates_and_missing_binary_refuse_before_extraction(self):
        for members in [
            [("../cargo-nextest", tarfile.REGTYPE, b"fixture")],
            [("cargo-nextest", tarfile.SYMTYPE, b"")],
            [("cargo-nextest", tarfile.LNKTYPE, b"")],
            [("cargo-nextest", tarfile.REGTYPE, b"a"), ("cargo-nextest", tarfile.REGTYPE, b"b")],
            [("license", tarfile.REGTYPE, b"original fixture")],
        ]:
            with self.subTest(members=members), self.assertRaises(RuntimeError):
                INSTALL["unpack_binary"](package(members))

    def test_verified_archive_and_version_produce_bound_receipt(self):
        archive = package([("cargo-nextest", tarfile.REGTYPE, b"original binary fixture")])
        profile = self.fixture(archive)
        with tempfile.TemporaryDirectory() as directory, patch("platform.system", return_value="Linux"), patch("platform.machine", return_value="x86_64"), \
             patch("urllib.request.urlopen", return_value=io.BytesIO(archive)), patch("subprocess.check_output", return_value="cargo-nextest 0.9.150 fixture") as version:
            result = INSTALL["install"](Path(directory), profile)
            self.assertEqual((Path(directory) / "cargo-nextest").read_bytes(), b"original binary fixture")
            self.assertEqual(result["binary_sha256"], hashlib.sha256(b"original binary fixture").hexdigest())
            self.assertEqual(result["archive_sha256"], profile["nextest"]["architectures"]["x86_64"]["archive_sha256"])
            self.assertEqual(json.loads((Path(directory) / "correctness-attestation.json").read_text()), result)
            version.assert_called_once()

    def test_download_size_or_hash_changes_never_write_executable(self):
        archive = package([("cargo-nextest", tarfile.REGTYPE, b"original fixture")])
        for change in ("size", "hash", "oversize", "version"):
            profile = self.fixture(archive)
            pin = profile["nextest"]["architectures"]["x86_64"]
            if change == "size": pin["archive_bytes"] += 1
            if change == "hash": pin["archive_sha256"] = "0" * 64
            if change == "oversize": pin["archive_bytes"] -= 1
            with tempfile.TemporaryDirectory() as directory, patch("platform.system", return_value="Linux"), patch("platform.machine", return_value="x86_64"), \
                 patch("urllib.request.urlopen", return_value=io.BytesIO(archive)), patch("subprocess.check_output", return_value="cargo-nextest WRONG"):
                with self.subTest(change=change), self.assertRaises(RuntimeError):
                    INSTALL["install"](Path(directory), profile)
                self.assertFalse((Path(directory) / "correctness-attestation.json").exists())
                if change != "version": self.assertFalse((Path(directory) / "cargo-nextest").exists())

    def test_existing_output_is_preserved_without_network_or_execution(self):
        archive = package([("cargo-nextest", tarfile.REGTYPE, b"original fixture")])
        with tempfile.TemporaryDirectory() as directory, patch("platform.system", return_value="Linux"), patch("platform.machine", return_value="x86_64"), patch("urllib.request.urlopen") as network:
            target = Path(directory) / "cargo-nextest"
            target.write_bytes(b"retain existing work")
            with self.assertRaisesRegex(RuntimeError, "Preserve existing"):
                INSTALL["install"](Path(directory), self.fixture(archive))
            self.assertEqual(target.read_bytes(), b"retain existing work")
            network.assert_not_called()

    def test_wrong_platform_or_redirected_pin_refuses_without_network(self):
        archive = package([("cargo-nextest", tarfile.REGTYPE, b"original fixture")])
        for change in ("platform", "url", "triple", "bool_size"):
            profile = self.fixture(archive)
            pin = profile["nextest"]["architectures"]["x86_64"]
            if change == "url": pin["url"] = "https://example.invalid/tool"
            if change == "triple": pin["triple"] = "other-unknown-linux-musl"
            if change == "bool_size": pin["archive_bytes"] = True
            with tempfile.TemporaryDirectory() as directory, patch("platform.system", return_value="Windows" if change == "platform" else "Linux"), \
                 patch("platform.machine", return_value="x86_64"), patch("urllib.request.urlopen") as network:
                with self.subTest(change=change), self.assertRaises(RuntimeError):
                    INSTALL["install"](Path(directory), profile)
                network.assert_not_called()
