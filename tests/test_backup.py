"""Backup drills use temporary archives and never touch a deployment."""

import hashlib
import importlib.util
import io
import os
import subprocess
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "verify_backup", "scripts/verify-backup.py"
)
backup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(backup)


class Backup(unittest.TestCase):
    def test_checksum_and_isolated_extraction_preserve_relative_current_link(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "snapshot.tar.gz"
            with tarfile.open(archive, "w:gz") as output:
                item = tarfile.TarInfo("data/images/releases/images-fixture/pic.png")
                item.size = 5
                output.addfile(item, io.BytesIO(b"image"))
                link = tarfile.TarInfo("data/images/current")
                link.type = tarfile.SYMTYPE
                link.linkname = "releases/images-fixture"
                output.addfile(link)
            checksum = Path(str(archive) + ".sha256")
            checksum.write_text(
                hashlib.sha256(archive.read_bytes()).hexdigest() + "  snapshot.tar.gz\n"
            )
            destination = root / "drill"
            backup.extract(archive, destination)
            self.assertEqual(
                (destination / "data/images/current/pic.png").read_bytes(), b"image"
            )
            with self.assertRaises(FileExistsError):
                backup.extract(archive, destination)
            checksum.write_text("0" * 64)
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                backup.extract(archive, root / "other")
            self.assertFalse((root / "other").exists())

    def test_snapshot_preserves_running_set_and_includes_all_roots(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            roots = {
                name: root / name
                for name in ("app", "data", "edge", "acme", "backups", "bin")
            }
            for directory in roots.values():
                directory.mkdir()
                (directory / "sentinel").write_text(directory.name)
            fake = roots["bin"] / "docker"
            fake.write_text(
                "#!/bin/sh\ncase \"$*\" in *'ps --status running --services'*) printf 'gitea\\nnginx\\n';; *) printf '%s\\n' \"$*\" >> \"$DOCKER_LOG\";; esac\n"
            )
            fake.chmod(0o755)
            env = dict(
                os.environ,
                PATH=str(roots["bin"]) + ":" + os.environ["PATH"],
                APP_ROOT=str(roots["app"]),
                DATA_ROOT=str(roots["data"]),
                TRAEFIK_CONFIG_ROOT=str(roots["edge"]),
                ACME_ROOT=str(roots["acme"]),
                BACKUP_ROOT=str(roots["backups"]),
                DOCKER_LOG=str(root / "docker.log"),
            )
            result = subprocess.run(
                ["sh", "scripts/snapshot.sh"], env=env, capture_output=True, text=True
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (root / "docker.log").read_text().splitlines(),
                ["compose stop gitea nginx", "compose start gitea nginx"],
            )
            archive = next(roots["backups"].glob("*.tar.gz"))
            backup.extract(archive, root / "drill")
            for name in ("app", "data", "edge", "acme"):
                self.assertEqual(
                    (
                        root / "drill" / str(roots[name]).lstrip("/") / "sentinel"
                    ).read_text(),
                    name,
                )
            env["BACKUP_ROOT"] = str(roots["app"] / "nested-backup")
            rejected = subprocess.run(
                ["sh", "scripts/snapshot.sh"], env=env, capture_output=True, text=True
            )
            self.assertNotEqual(rejected.returncode, 0)

    def test_outside_symlinks_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "snapshot.tar.gz"
            with tarfile.open(archive, "w:gz") as output:
                link = tarfile.TarInfo("escape")
                link.type = tarfile.SYMTYPE
                link.linkname = "../outside"
                output.addfile(link)
            Path(str(archive) + ".sha256").write_text(
                hashlib.sha256(archive.read_bytes()).hexdigest()
            )
            with self.assertRaises(tarfile.LinkOutsideDestinationError):
                backup.extract(archive, root / "drill")
            self.assertFalse((root / "outside").exists())
