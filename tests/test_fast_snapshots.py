"""Direct snapshots, wakeup recovery and verified physical deduplication."""

import json
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from open_assets import __main__ as core, local_dedup, queue_wakeup, resources
import test_incremental as fixture
import test_reference_publication as refs


class FastSnapshot(unittest.TestCase):
    setUp = refs.ReferencePublication.setUp
    git = fixture.Incremental.git
    publish = fixture.Incremental.publish

    def test_single_tree_and_unchanged_inode(self):
        first, _ = self.publish()
        second, job = self.publish()
        self.assertEqual(job["status"], "published")
        self.assertFalse((self.base / "releases" / ("images-" + second)).exists())
        self.assertEqual(
            (self.base / "current").readlink().as_posix(),
            "releases/" + second + "/images",
        )
        self.assertEqual(
            (self.base / "current/a.png").stat().st_ino,
            (self.base / "releases" / first / "images/a.png").stat().st_ino,
        )
        core.rollback_images(self.config, "assets", first)
        self.assertEqual((self.base / "current/a.png").read_bytes(), b"a")

    def test_dedup_rebuilds_only_local_proof_without_reupload(self):
        self.publish()
        self.publish()
        before = self.store.puts
        report = local_dedup.run(self.config, apply=True)
        self.assertEqual(report["seals_refreshed"], 1)
        _, job = self.publish()
        self.assertEqual(job["incremental"]["changed_files"], 0)
        self.assertEqual(self.store.puts, before)
        self.assertEqual(job["incremental"]["object_transfers"]["referenced"], 3)

    def test_materialize_legacy_view_preserves_bytes_and_rollback(self):
        sha, _ = self.publish()
        self.assertEqual(local_dedup.legacy_views(self.config)["versions"], 1)
        local_dedup.legacy_views(self.config, apply=True)
        self.assertEqual(
            (self.base / "current").readlink().as_posix(), "releases/images-" + sha
        )
        self.assertEqual(
            (self.base / "current/a.png").stat().st_ino,
            (self.base / "releases" / sha / "images/a.png").stat().st_ino,
        )
        core.rollback_images(self.config, "assets", sha)
        self.assertEqual(core.active_image_state(self.config, "assets", sha), "current")

    def test_concurrent_remote_change_never_activates_stale_commit(self):
        first, _ = self.publish()
        with patch.object(resources, "remote_head", return_value="f" * 40):
            _, job = self.publish()
        self.assertEqual(job["status"], "superseded")
        self.assertEqual(
            core.active_image_state(self.config, "assets", first), "current"
        )

    def test_advertisement_must_match_exact_branch(self):
        for output in (
            b"",
            b"a" * 40 + b"\trefs/heads/other\n",
            (b"a" * 40 + b"\trefs/heads/main\n") * 2,
        ):
            with (
                patch.object(resources, "run_git", return_value=output),
                self.assertRaises(ValueError),
            ):
                resources.remote_head(
                    self.config, self.config["repositories"][0], self.source
                )


class Dedup(unittest.TestCase):
    def setUp(self):
        self.f = fixture.Incremental()
        self.f.setUp()
        self.addCleanup(self.f.doCleanups)
        self.first, _ = self.f.publish()
        self.second, _ = self.f.publish()

    def test_dry_run_then_shared_bytes_and_rollback(self):
        f = self.f
        first = f.base / "releases" / self.first / "images/a.png"
        second = f.base / "releases" / self.second / "images/a.png"
        self.assertNotEqual(first.stat().st_ino, second.stat().st_ino)
        preview = local_dedup.run(f.config)
        self.assertGreater(preview["reclaimable_bytes"], 0)
        self.assertNotEqual(first.stat().st_ino, second.stat().st_ino)
        result = local_dedup.run(f.config, apply=True)
        self.assertGreater(result["replaced"], 0)
        self.assertEqual(first.stat().st_ino, second.stat().st_ino)
        self.assertEqual(second.stat().st_mode & 0o222, 0)
        self.assertEqual(local_dedup.run(f.config)["reclaimable_bytes"], 0)
        for sha in (self.first, self.second):
            core.rollback_images(f.config, "assets", sha)
            self.assertEqual((f.base / "current/a.png").read_bytes(), b"a")

    def test_corruption_prevents_every_replacement(self):
        f = self.f
        target = f.base / "releases" / self.second / "images/a.png"
        target.write_bytes(b"x")
        before = (f.base / "releases" / self.first / "images/b.png").stat().st_ino
        with self.assertRaises(ValueError):
            local_dedup.run(f.config, apply=True)
        self.assertEqual(
            before, (f.base / "releases" / self.first / "images/b.png").stat().st_ino
        )
        self.assertEqual(target.read_bytes(), b"x")

    def test_active_queue_prevents_mutation(self):
        f = self.f
        core.enqueue(f.config, "assets", "f" * 40, "fixture")
        with self.assertRaises(ValueError):
            local_dedup.run(f.config, apply=True)


class Wakeup(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux", "inotify requires Linux")
    def test_atomic_enqueue_wakes_wait_and_event_before_wait_is_kept(self):
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory)
            w = queue_wakeup.QueueWakeup(p)
            self.addCleanup(w.close)
            self.assertGreaterEqual(w.fd, 0)
            core.atomic(p / "job.json", dict(status="pending"))
            started = time.monotonic()
            w.wait(1)
            self.assertLess(time.monotonic() - started, 0.2)
            w.drain()
            started = time.monotonic()
            w.wait(0.03)
            self.assertGreaterEqual(time.monotonic() - started, 0.02)
            self.assertEqual(
                json.loads((p / "job.json").read_text())["status"], "pending"
            )

    def test_notification_failure_keeps_periodic_scan(self):
        with patch.object(queue_wakeup.sys, "platform", "other"):
            w = queue_wakeup.QueueWakeup(Path("/unused"))
        self.assertEqual(w.fd, -1)
        w.drain()
        w.wait(0.001)
        w.close()
