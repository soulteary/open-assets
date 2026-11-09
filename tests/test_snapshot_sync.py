"""Directory durability barriers survive parallel sibling synchronization."""

import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

from open_assets import __main__ as core


class SnapshotSync(unittest.TestCase):
    def test_every_parent_waits_for_its_children(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative in ("a/b", "a/c", "x/y/z"):
                (root / relative).mkdir(parents=True)
            folders = [root, *[p for p in root.rglob("*") if p.is_dir()]]
            required = {
                p.stat().st_ino: {c.stat().st_ino for c in p.iterdir() if c.is_dir()}
                for p in folders
            }
            completed = set()
            lock = threading.Lock()

            def sync(fd):
                inode = os.fstat(fd).st_ino
                with lock:
                    self.assertTrue(required[inode] <= completed)
                    completed.add(inode)

            with patch.object(os, "fsync", side_effect=sync):
                core.sync_tree(root, data_synced=True)
            self.assertEqual(completed, set(required))

    def test_sync_failure_prevents_rename_or_activation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "nested").mkdir()
            with (
                patch.object(os, "fsync", side_effect=OSError("fixture")),
                self.assertRaises(OSError),
            ):
                core.sync_tree(root, data_synced=True)
