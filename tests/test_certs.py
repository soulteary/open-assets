import hashlib
import importlib.util
import io
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "certs_maker", Path("scripts/certs_maker.py")
)
maker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(maker)


class Certificates(unittest.TestCase):
    def archive(self, name="certs-maker"):
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode="w:gz") as archive:
            member = tarfile.TarInfo(name)
            member.size = 4
            archive.addfile(member, io.BytesIO(b"test"))
        return stream.getvalue()

    def test_verified_archive_and_modified_binary_rejection(self):
        with (
            tempfile.TemporaryDirectory() as tmp,
            patch.object(maker.platform, "system", return_value="Linux"),
            patch.object(maker.platform, "machine", return_value="x86_64"),
        ):
            root = Path(tmp)
            (root / "private").mkdir()
            data = self.archive()
            source = root / "archive"
            source.write_bytes(data)
            settings = dict(
                CERTS_MAKER_VERSION="3.8.2",
                CERTS_MAKER_SHA256_AMD64=hashlib.sha256(data).hexdigest(),
            )
            binary = maker.ensure_binary(root, settings, source)
            self.assertEqual(binary.read_bytes(), b"test")
            self.assertEqual(maker.ensure_binary(root, settings, source), binary)
            binary.write_bytes(b"changed")
            with self.assertRaises(ValueError):
                maker.ensure_binary(root, settings, source)

    def test_bad_checksum_and_traversal_entry_rejected(self):
        for traversal in (False, True):
            with (
                self.subTest(traversal=traversal),
                tempfile.TemporaryDirectory() as tmp,
                patch.object(maker.platform, "system", return_value="Linux"),
                patch.object(maker.platform, "machine", return_value="x86_64"),
            ):
                root = Path(tmp)
                (root / "private").mkdir()
                data = self.archive("../certs-maker" if traversal else "certs-maker")
                source = root / "archive"
                source.write_bytes(data)
                settings = dict(
                    CERTS_MAKER_VERSION="3.8.2",
                    CERTS_MAKER_SHA256_AMD64=hashlib.sha256(data).hexdigest()
                    if traversal
                    else "0" * 64,
                )
                with self.assertRaises(ValueError):
                    maker.ensure_binary(root, settings, source)
                self.assertFalse((root / "private/tools/certs-maker").exists())

    def test_generation_refuses_live_directory(self):
        with patch.dict(sys.modules, {"certs_maker": maker}):
            spec = importlib.util.spec_from_file_location(
                "internal_certs", Path("scripts/internal_certs.py")
            )
            certs = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(certs)
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)
            (path / "ca.crt").write_text("existing")
            with self.assertRaises(ValueError):
                certs.generate(path, {}, Path("unused"))
            self.assertEqual((path / "ca.crt").read_text(), "existing")
