"""Retention fixtures use only disposable local data; no object-store access."""

import copy
import fcntl
import hashlib
import importlib
import json
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

from open_assets import retention as r

core = importlib.import_module("open_assets.__main__")


class Retention(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.now = time.time()
        self.config = dict(
            state_root=str(self.root / "state"),
            images_root=str(self.root / "images"),
            revision="fixture",
            retention=dict(
                enabled=True,
                keep_last=1,
                keep_days=7,
                min_age_hours=1,
                rollback_versions=2,
                archive_jobs_after_days=7,
            ),
            repositories=[dict(key="assets", enabled=True)],
        )
        self.base = self.root / "images/repos/assets"
        self.state = self.root / "state"
        self.shas = [hashlib.sha1(str(i).encode()).hexdigest() for i in range(9)]
        for i, sha in enumerate(self.shas):
            self.version(sha, 40 - i)
        self.current = self.shas[-1]
        core.switch(self.base, "images-" + self.current)
        core.atomic(
            self.state / "published/assets", dict(key="assets", sha=self.current)
        )

    def tearDown(self):
        self.tmp.cleanup()

    def version(self, sha, days):
        full = self.base / "releases" / sha
        image = self.base / "releases" / ("images-" + sha)
        (full / "images").mkdir(parents=True)
        image.mkdir()
        data = sha.encode()
        (full / "images/pic.png").write_bytes(data)
        (image / "pic.png").write_bytes(data)
        manifest = self.state / "manifests/assets" / (sha + ".json")
        core.atomic(
            manifest,
            dict(
                key="assets",
                sha=sha,
                files=[
                    dict(
                        path="images/pic.png",
                        size=len(data),
                        sha256=hashlib.sha256(data).hexdigest(),
                    )
                ],
            ),
        )
        for path in [*full.rglob("*"), *image.rglob("*"), full, image, manifest]:
            os.utime(path, (self.now - days * 86400,) * 2)

    def job(self, sha, status, days=30, **extra):
        jid = core.enqueue(self.config, "assets", sha, "fixture")
        path = self.state / "jobs" / (jid + ".json")
        value = json.loads(path.read_text())
        value.update(status=status, created=self.now - days * 86400, **extra)
        core.atomic(path, value)
        os.utime(path, (self.now - days * 86400,) * 2)
        return jid

    def plan(self, apply=False):
        return r.run(self.config, "assets", apply, self.now)["repositories"][0]

    def test_preview_and_default_disabled_do_not_delete(self):
        before = set((self.base / "releases").iterdir())
        result = self.plan()
        self.assertTrue(result["candidates"])
        self.assertEqual(set((self.base / "releases").iterdir()), before)
        self.config["retention"]["enabled"] = False
        result = self.plan(True)
        self.assertNotIn("trash_removed", result)
        self.assertEqual(set((self.base / "releases").iterdir()), before)

    def test_current_rollback_pins_and_all_inflight_states_protected(self):
        core.atomic(
            r.history_path(self.config, "assets"),
            dict(key="assets", shas=[self.shas[0]]),
        )
        self.config["retention"]["pinned_shas"] = [self.shas[1]]
        for sha, status in zip(
            self.shas[2:6], ("pending", "running", "images_prepared", "failed")
        ):
            self.job(sha, status)
        result = self.plan(True)
        protected = {self.current, *self.shas[:6]}
        self.assertFalse(protected & {e["sha"] for e in result["candidates"]})
        for sha in protected:
            self.assertTrue((self.base / "releases" / ("images-" + sha)).exists())
        self.assertFalse((self.base / "releases" / self.shas[6]).exists())

    def test_previous_job_bootstrap_and_rollback_history_updates(self):
        self.job(self.current, "published", previous="releases/images-" + self.shas[0])
        self.assertIn("previous-job", self.plan()["protected"][self.shas[0]])
        r.remember_previous(self.config, "assets", self.shas[1])
        self.assertEqual(r.history(self.config, "assets"), [self.current])
        core.switch(self.base, "images-" + self.shas[1])
        r.remember_previous(self.config, "assets", self.shas[2])
        self.assertEqual(r.history(self.config, "assets"), [self.shas[1], self.current])

    def test_archive_preserves_deduplication_and_terminal_bytes(self):
        jid = self.job(self.shas[0], "published", days=50)
        original = (self.state / "jobs" / (jid + ".json")).read_bytes()
        self.plan(True)
        archived = r.archive_path(self.config, jid)
        self.assertEqual(archived.read_bytes(), original)
        self.assertEqual(
            core.enqueue(self.config, "assets", self.shas[0], "repeat"), jid
        )
        self.assertFalse((self.state / "jobs" / (jid + ".json")).exists())
        manual = core.enqueue(
            self.config, "assets", self.shas[0], "explicit", kind="manual"
        )
        self.assertNotEqual(jid, manual)
        self.assertTrue(
            (self.state / "manifests/assets" / (self.shas[0] + ".json")).exists()
        )

    def test_keep_count_age_and_minimum_age_are_unioned(self):
        self.config["retention"].update(keep_last=2, keep_days=35)
        result = self.plan()
        for sha in self.shas[-3:]:
            self.assertIn(sha, result["protected"])
        self.config["retention"].update(keep_days=0, min_age_hours=1000)
        self.assertFalse(self.plan()["candidates"])

    def test_invalid_live_job_or_reference_blocks_before_any_mutation(self):
        (self.state / "jobs").mkdir()
        (self.state / "jobs/bad.json").write_text("{")
        with self.assertRaises(ValueError):
            self.plan(True)
        self.assertTrue((self.base / "releases" / self.shas[0]).exists())
        (self.state / "jobs/bad.json").unlink()
        core.atomic(
            self.state / "published/assets", dict(key="assets", sha=self.shas[0])
        )
        with self.assertRaises(ValueError):
            self.plan(True)
        self.assertTrue((self.base / "releases" / self.shas[0]).exists())

    def test_symlink_and_malformed_history_fail_closed(self):
        core.atomic(r.history_path(self.config, "assets"), dict(key="wrong", shas=[]))
        with self.assertRaises(ValueError):
            self.plan(True)
        r.history_path(self.config, "assets").unlink()
        (self.base / "releases" / self.shas[0] / "linked").symlink_to(self.root)
        with self.assertRaises(ValueError):
            self.plan(True)
        self.assertTrue((self.base / "releases" / self.shas[1]).exists())

    def test_budget_bounds_new_claims_and_deletion(self):
        self.config["retention"].update(max_delete_versions=1, max_delete_bytes=80)
        first = self.plan(True)
        self.assertEqual(len(first["candidates"]), 1)
        self.assertEqual(first["trash_removed"], 1)
        second = self.plan(True)
        self.assertEqual(len(second["candidates"]), 1)
        self.assertNotEqual(
            first["candidates"][0]["sha"], second["candidates"][0]["sha"]
        )

    def test_repeat_and_other_repository_are_safe(self):
        other = self.root / "images/repos/other/releases"
        other.mkdir(parents=True)
        (other / "unmanaged").write_text("retain")
        self.plan(True)
        second = self.plan(True)
        self.assertFalse(second["candidates"])
        self.assertEqual((other / "unmanaged").read_text(), "retain")
        self.assertEqual(
            os.readlink(self.base / "current"), "releases/images-" + self.current
        )

    def test_worker_queue_and_cleaner_locks_return_busy(self):
        for name in ("worker", "queue", "retention"):
            with (self.state / (name + ".lock")).open("a") as stream:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.assertEqual(r.run(self.config, apply=True)["status"], "busy")
        self.assertTrue((self.base / "releases" / self.shas[0]).exists())

    def test_interrupted_claim_resumes_only_trash_not_recreated_live_version(self):
        original = os.rename
        calls = 0

        def fail_second(source, target):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("interrupted second directory rename")
            original(source, target)

        self.config["retention"]["max_delete_versions"] = 1
        with patch.object(r.os, "rename", side_effect=fail_second):
            with self.assertRaises(OSError):
                self.plan(True)
        sha = self.shas[0]
        # A new publication reconstructs and activates the formerly claimed SHA.
        full = self.base / "releases" / sha
        full.mkdir()
        (full / "new.txt").write_text("new publication")
        core.switch(self.base, "images-" + sha)
        core.atomic(self.state / "published/assets", dict(key="assets", sha=sha))
        self.config["retention"]["enabled"] = False
        self.assertTrue(list((self.base / ".retention-trash").iterdir()))
        self.plan(True)
        self.config["retention"]["enabled"] = True
        self.plan(True)
        self.assertEqual((full / "new.txt").read_text(), "new publication")
        self.assertTrue((self.base / "releases" / ("images-" + sha)).exists())

    def test_missing_manifest_is_preserved_and_apply_recomputes_protection(self):
        sha = self.shas[0]
        (self.state / "manifests/assets" / (sha + ".json")).unlink()
        self.assertIn("missing-manifest", self.plan()["protected"][sha])
        old_plan = self.plan()
        candidate = old_plan["candidates"][0]["sha"]
        self.config["retention"]["pinned_shas"] = [candidate]
        self.plan(True)
        self.assertTrue((self.base / "releases" / candidate).exists())
        self.assertTrue((self.base / "releases" / sha).exists())
        self.assertEqual(
            json.loads((self.state / "retention-last.json").read_text())["status"],
            "complete",
        )

    def test_bad_claim_receipt_and_trash_symlink_cannot_delete_outside(self):
        trash = self.base / ".retention-trash"
        trash.mkdir()
        folder = trash / ("a" * 32)
        folder.mkdir()
        core.atomic(
            folder / "receipt.json",
            dict(schema_version=1, key="assets", sha="../escape"),
        )
        with self.assertRaises(ValueError):
            r.drain_trash(self.config, self.config["repositories"][0])
        core.atomic(
            folder / "receipt.json",
            dict(schema_version=1, key="assets", sha=self.shas[0]),
        )
        outside = self.root / "outside"
        outside.mkdir()
        (outside / "keep").write_text("safe")
        (folder / "full").symlink_to(outside, target_is_directory=True)
        with self.assertRaises(ValueError):
            r.drain_trash(self.config, self.config["repositories"][0])
        self.assertEqual((outside / "keep").read_text(), "safe")

    def test_reactivation_refreshes_age_without_modifying_file_bytes(self):
        sha = self.shas[0]
        image = self.base / "releases" / ("images-" + sha) / "pic.png"
        original = image.read_bytes()
        r.remember_previous(self.config, "assets", sha)
        result = self.plan()
        self.assertIn("recent", result["protected"][sha])
        self.assertEqual(image.read_bytes(), original)

    def test_planning_timeout_stops_before_claiming_versions(self):
        with patch.object(r.time, "monotonic", side_effect=[0, 100]):
            with self.assertRaises(TimeoutError):
                self.plan(True)
        self.assertTrue((self.base / "releases" / self.shas[0]).exists())
        self.assertFalse((self.base / ".retention-trash").exists())

    def test_active_job_scan_limit_stops_before_deletion(self):
        self.job(self.shas[0], "published", days=50)
        self.job(self.shas[1], "superseded", days=50)
        with patch.object(r, "MAX_ACTIVE_JOBS", 1):
            with self.assertRaises(ValueError):
                self.plan(True)
        self.assertTrue((self.base / "releases" / self.shas[0]).exists())
        self.assertFalse((self.base / ".retention-trash").exists())

    def test_missing_current_preserves_all_versions_and_null_published_is_invalid(self):
        (self.base / "current").unlink()
        published = self.state / "published/assets"
        published.unlink()
        before = set((self.base / "releases").iterdir())
        result = self.plan(True)
        self.assertFalse(result["candidates"])
        self.assertEqual(set((self.base / "releases").iterdir()), before)
        core.atomic(published, dict(key="assets", sha=None))
        with self.assertRaises(ValueError):
            self.plan(True)
        self.assertEqual(set((self.base / "releases").iterdir()), before)

    def test_policy_rejects_unknown_or_unsafe_values_and_keeps_repo_override(self):
        for change in (
            dict(keep_last=0),
            dict(min_age_hours=0),
            dict(enabled="yes"),
            dict(pinned_shas=["../escape"]),
            dict(unknown=1),
        ):
            candidate = copy.deepcopy(self.config)
            candidate["retention"].update(change)
            with self.assertRaises(ValueError):
                r.policy(candidate, candidate["repositories"][0])
        self.config["repositories"][0]["retention"] = dict(enabled=False, keep_last=3)
        self.assertFalse(
            r.policy(self.config, self.config["repositories"][0])["enabled"]
        )


if __name__ == "__main__":
    unittest.main()
