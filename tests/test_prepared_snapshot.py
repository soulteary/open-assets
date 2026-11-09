"""A warm view changes only private links, preserving byte proofs and rollback."""

import errno
import hashlib
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

from open_assets import (
    __main__ as core,
    immutable,
    incremental,
    object_refs,
    prepared_snapshot,
)
import test_reference_publication as fixture


class PreparedSnapshot(unittest.TestCase):
    def case(self):
        case = fixture.ReferencePublication()
        case.setUp()
        self.addCleanup(case.doCleanups)
        for index in range(48):
            p = case.source / "public/images/photos" / f"{index:03}" / "image.png"
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(f"file-{index}".encode())
        first, job = case.publish()
        self.assertEqual(job["status"], "published")
        return case, first

    def warm(self, case):
        with core.lock(case.config):
            prepared_snapshot.prepare_idle(case.config)
        record = object_refs.read(prepared_snapshot.state_path(case.config, "assets"))
        self.assertEqual(record["status"], "ready")
        return record, prepared_snapshot.candidate(
            case.config, case.config["repositories"][0]
        )["_root"]

    def test_idle_view_shares_bytes_and_does_not_extend_original_proofs(self):
        case, first = self.case()
        path = Path(case.config["state_root"]) / "incremental/assets.json"
        before = object_refs.read(path)
        record, root = self.warm(case)
        after = object_refs.read(path)
        for name, value in before["records"].items():
            self.assertEqual(
                value["immutable"]["verified_at"],
                after["records"][name]["immutable"]["verified_at"],
            )
        source = case.base / "releases" / first / "images/photos/004/image.png"
        ready = root / "images/photos/004/image.png"
        self.assertEqual(source.stat().st_ino, ready.stat().st_ino)
        self.assertEqual(ready.stat().st_mode & 0o222, 0)
        self.assertTrue(
            incremental.context(case.config, case.config["repositories"][0]).get(
                "prepared"
            )
        )
        self.assertEqual(
            len(
                incremental.context(
                    case.config, case.config["repositories"][0], force_current=True
                )["sources"]
            ),
            len(record["files"]),
        )

    def test_delta_prunes_empty_directories_and_only_syncs_changed_ancestors(self):
        case, first = self.case()
        record, _ = self.warm(case)
        puts = case.store.puts
        photos = case.source / "public/images/photos"
        (photos / "000/image.png").write_bytes(b"changed")
        (photos / "001/image.png").unlink()
        (photos / "renamed").mkdir()
        (photos / "002/image.png").rename(photos / "renamed/image.png")
        second, job = case.publish()
        self.assertEqual(job["status"], "published")
        self.assertTrue(job["snapshot"]["prepared"])
        self.assertLess(job["snapshot"]["synced_directories"], 12)
        self.assertEqual(case.store.puts, puts + 1)
        manifest = case.manifest(second)
        for item in manifest["files"]:
            body = (case.base / "releases" / second / item["path"]).read_bytes()
            self.assertEqual(hashlib.sha256(body).hexdigest(), item["sha256"])
        self.assertFalse((case.base / "current/photos/001").exists())
        for sha, body in ((first, b"file-0"), (second, b"changed")):
            core.rollback_images(case.config, "assets", sha)
            self.assertEqual(
                (case.base / "current/photos/000/image.png").read_bytes(), body
            )
        self.assertEqual(
            (case.base / "releases" / first / "images/photos/004/image.png")
            .stat()
            .st_ino,
            (case.base / "releases" / second / "images/photos/004/image.png")
            .stat()
            .st_ino,
        )
        self.warm(case)
        self.assertEqual(len(list(case.base.glob(".staging-prefetch-*"))), 1)

    def test_invalid_view_falls_back_without_publishing_unlisted_files(self):
        case, _ = self.case()
        _, root = self.warm(case)
        (root / "unlisted.txt").write_bytes(b"untrusted")
        puts = case.store.puts
        _, job = case.publish()
        self.assertEqual(job["status"], "published")
        self.assertNotIn("snapshot", job)
        self.assertEqual(case.store.puts, puts)
        self.assertFalse((case.base / "current/unlisted.txt").exists())

    def test_dirty_sync_failure_leaves_previous_current(self):
        case, first = self.case()
        self.warm(case)
        (case.source / "public/images/a.png").write_bytes(b"updated")
        original = core.sync_tree

        def sync(root, data_synced=False, directories=None):
            if directories is not None:
                raise OSError("sync failed")
            return original(root, data_synced=data_synced)

        with patch.object(core, "sync_tree", side_effect=sync):
            _, job = case.publish()
        self.assertEqual(job["status"], "failed")
        self.assertEqual(
            (case.base / "current").readlink().as_posix(),
            "releases/" + first + "/images",
        )
        self.assertEqual((case.base / "current/a.png").read_bytes(), b"a")

    def test_view_only_renews_proofs_after_real_byte_reads(self):
        case, _ = self.case()
        record, _ = self.warm(case)
        path = prepared_snapshot.state_path(case.config, "assets")
        seal_path = Path(case.config["state_root"]) / "incremental/assets.json"
        seal = object_refs.read(seal_path)
        old = time.time() - 2000
        for proof in record["proofs"].values():
            proof["verified_at"] = old
        for value in seal["records"].values():
            value["immutable"]["verified_at"] = old
        core.atomic(path, record)
        core.atomic(seal_path, seal)
        with patch.object(incremental, "_audit_next", 0):
            incremental.audit_idle(case.config)
        refreshed = object_refs.read(path)
        self.assertTrue(
            all(p["verified_at"] > old for p in refreshed["proofs"].values())
        )
        self.assertTrue(
            incremental.context(case.config, case.config["repositories"][0]).get(
                "prepared"
            )
        )

    def test_optional_view_never_copies_when_hardlinks_are_unavailable(self):
        case, _ = self.case()
        with patch.object(
            immutable.os, "link", side_effect=OSError(errno.EXDEV, "unavailable")
        ):
            with core.lock(case.config):
                prepared_snapshot.prepare_idle(case.config)
        path = prepared_snapshot.state_path(case.config, "assets")
        record = object_refs.read(path)
        self.assertEqual(record["status"], "building")
        root = case.base / record["directory"]
        self.assertFalse(any(p.is_file() for p in root.rglob("*")))
        with core.lock(case.config):
            prepared_snapshot.prepare_idle(case.config)
        self.assertEqual(len(list(case.base.glob(".staging-prefetch-*"))), 1)

    def test_unlink_refuses_symlinks_and_detects_extra_link_changes(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            source = root / "source"
            source.write_bytes(b"safe")
            item = {"size": 4, "sha256": hashlib.sha256(b"safe").hexdigest()}
            proof = immutable.record(source, item)
            view = root / "view"
            view.symlink_to(source)
            with self.assertRaises(ValueError):
                immutable.unlink_view(view, source, proof)
            view.unlink()
            immutable.link(source, view, proof)
            original = immutable.os.unlink

            def unlink(path):
                original(path)
                immutable.os.link(source, root / "unexpected")

            with patch.object(immutable.os, "unlink", side_effect=unlink):
                with self.assertRaisesRegex(ValueError, "changed while unlinking"):
                    immutable.unlink_view(view, source, proof)

    def test_pending_publication_takes_priority_over_idle_preparation(self):
        case, _ = self.case()
        core.enqueue(case.config, "assets", "f" * 40, "priority")
        with core.lock(case.config):
            prepared_snapshot.prepare_idle(case.config)
        self.assertFalse(prepared_snapshot.state_path(case.config, "assets").exists())
        self.assertFalse(list(case.base.glob(".staging-prefetch-*")))

    def test_dirty_sync_rejects_paths_outside_staging(self):
        with tempfile.TemporaryDirectory() as folder:
            with self.assertRaises(ValueError):
                core.sync_tree(
                    Path(folder), data_synced=True, directories={Path("../")}
                )
