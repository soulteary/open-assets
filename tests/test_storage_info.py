"""Host measurements use disposable directories and never contact storage APIs."""

import copy
import hashlib
import json
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

import yaml

from open_assets import storage_info as s
from open_assets import telemetry
from types import SimpleNamespace


class StorageInfo(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "app"
        self.data = Path(self.tmp.name) / "data"
        (self.root / "private").mkdir(parents=True)
        self.config = dict(
            schema_version=1,
            state_root="/state",
            images_root="/images",
            retention=dict(enabled=True, keep_last=5, pinned_shas=["a" * 40]),
            repositories=[
                dict(
                    key="assets",
                    remote="ssh://secret-host.invalid/private.git",
                    password="must-not-export",
                    max_bytes=1073741824,
                )
            ],
        )
        self.save_config()
        self.base = self.data / "images/repos/assets"
        self.sha = hashlib.sha1(b"fixture").hexdigest()
        full = self.base / "releases" / self.sha
        image = self.base / "releases" / ("images-" + self.sha)
        (full / "images").mkdir(parents=True)
        image.mkdir()
        (full / "images/pic.png").write_bytes(b"picture")
        (image / "pic.png").write_bytes(b"picture")
        (self.base / "current").symlink_to("releases/images-" + self.sha)
        git = self.data / "publisher/git/assets"
        git.mkdir(parents=True)
        (git / "object").write_bytes(b"git-object")
        manifests = self.data / "publisher/manifests/assets"
        manifests.mkdir(parents=True)
        (manifests / (self.sha + ".json")).write_text('{"private-field":"ignored"}')

    def tearDown(self):
        self.tmp.cleanup()

    def save_config(self):
        (self.root / "private/repositories.yaml").write_text(
            yaml.safe_dump(self.config)
        )

    def collect(self, force=False):
        return s.collect_storage(self.root, self.data, force)

    def test_direct_snapshot_is_complete_and_not_a_second_physical_copy(self):
        import shutil

        image = self.base / "releases" / ("images-" + self.sha)
        shutil.rmtree(image)
        (self.base / "current").unlink()
        (self.base / "current").symlink_to("releases/" + self.sha + "/images")
        manifest = self.data / "publisher/manifests/assets" / (self.sha + ".json")
        manifest.write_text(
            json.dumps(dict(key="assets", sha=self.sha, image_layout="snapshot-v1"))
        )
        result = self.collect(True)
        row = result["repositories"][0]
        self.assertEqual(row["full_copies"], 1)
        self.assertEqual(row["image_copies"], 0)
        self.assertEqual(row["shared_image_views"], 1)
        self.assertEqual(row["partial_versions"], 0)
        self.assertEqual(result["status"], "ok")

    def test_host_snapshot_reads_cache_without_repeating_large_scan(self):
        self.collect(True)
        cache = self.data / "info/repositories-cache.json"
        before = cache.read_bytes()
        with patch.object(
            s, "measure", side_effect=AssertionError("host refresh must not scan")
        ):
            result = s.collect_storage(self.root, self.data, measure_now=False)
        self.assertEqual(result["status"], "ok")
        self.assertEqual(cache.read_bytes(), before)

    def test_counts_allocated_bytes_policy_and_allowlist(self):
        result = self.collect(True)
        self.assertEqual(result["status"], "ok")
        row = result["repositories"][0]
        self.assertEqual(
            (
                row["versions"],
                row["full_copies"],
                row["image_copies"],
                row["partial_versions"],
            ),
            (1, 1, 1, 0),
        )
        self.assertGreaterEqual(
            row["logical_bytes"], 2 * len(b"picture") + len(b"git-object")
        )
        self.assertEqual(
            row["local_bytes"],
            sum(
                row[key]
                for key in (
                    "version_bytes",
                    "git_bytes",
                    "manifest_bytes",
                    "trash_bytes",
                    "staging_bytes",
                    "other_bytes",
                )
            ),
        )
        self.assertEqual(row["policy"]["pinned_count"], 1)
        self.assertEqual(row["publication_required_bytes"], 2147483648)
        text = json.dumps(result)
        for secret in (
            "must-not-export",
            "secret-host",
            self.sha,
            "a" * 40,
            str(self.root),
            str(self.data),
            "private-field",
        ):
            self.assertNotIn(secret, text)

    def test_symlinks_are_not_followed_and_partial_versions_are_counted(self):
        original = self.collect(True)["repositories"][0]["logical_bytes"]
        outside = Path(self.tmp.name) / "outside"
        outside.mkdir()
        (outside / "secret").write_bytes(b"x" * 100000)
        (self.base / "external").symlink_to(outside, target_is_directory=True)
        extra = self.base / "releases" / ("b" * 40)
        extra.mkdir()
        row = self.collect(True)["repositories"][0]
        self.assertEqual(row["logical_bytes"], original)
        self.assertEqual(
            (row["versions"], row["partial_versions"], row["unusual_entries"]),
            (2, 1, 1),
        )

    def test_cache_used_and_publication_or_config_changes_refresh(self):
        self.collect(True)
        with patch.object(s, "measure", side_effect=AssertionError("cache expected")):
            self.assertEqual(self.collect()["status"], "ok")
        extra = self.base / "releases" / ("b" * 40)
        extra.mkdir()
        self.assertEqual(self.collect()["repositories"][0]["versions"], 2)
        self.config["retention"]["keep_last"] = 10
        self.save_config()
        self.assertEqual(self.collect()["repositories"][0]["policy"]["keep_last"], 10)

    def test_scan_failure_keeps_old_values_marks_stale_and_retries(self):
        old = self.collect(True)["repositories"][0]
        with patch.object(s, "measure", side_effect=TimeoutError("budget")):
            failed = self.collect(True)["repositories"][0]
        self.assertEqual(failed["local_bytes"], old["local_bytes"])
        self.assertEqual(failed["measurement_status"], "unavailable")
        self.assertTrue(failed["measurement_stale"])
        original = s.measure
        with patch.object(s, "measure", wraps=original) as measure:
            self.assertEqual(self.collect()["status"], "ok")
            self.assertEqual(measure.call_count, 1)

    def test_budget_and_linked_root_report_unknown_without_zero(self):
        with patch.object(s, "MAX_ENTRIES", 1):
            result = self.collect(True)
        self.assertEqual(result["status"], "partial")
        self.assertNotIn("local_bytes", result["repositories"][0])
        git = self.data / "publisher/git/assets"
        (git / "object").unlink()
        git.rmdir()
        git.symlink_to(self.root / "private", target_is_directory=True)
        self.assertEqual(
            self.collect(True)["repositories"][0]["measurement_status"], "unavailable"
        )

    def test_bad_config_or_cache_cannot_export_unknown_fields(self):
        self.collect(True)
        cache = self.data / "info/repositories-cache.json"
        value = json.loads(cache.read_text())
        value["repositories"]["assets"]["password"] = "cached-secret"
        cache.write_text(json.dumps(value))
        self.assertNotIn("cached-secret", json.dumps(self.collect()))
        self.config["images_root"] = "/private"
        self.save_config()
        self.assertEqual(self.collect()["status"], "unavailable")
        self.assertFalse(s.finite_time(10**500))

    def test_cleanup_counts_are_summary_only_and_disabled_is_not_deleted(self):
        report = self.data / "publisher/retention-last.json"
        source = dict(
            status="complete",
            apply=True,
            completed_at=time.time(),
            repositories=[
                dict(
                    key="assets",
                    enabled=True,
                    candidates=[dict(secret="do-not-export")],
                    archive_jobs=["private-id"],
                    trash_removed=2,
                    unknown="hidden",
                )
            ],
        )
        report.write_text(json.dumps(source))
        cleanup = self.collect(True)["repositories"][0]["cleanup"]
        self.assertEqual(
            (
                cleanup["claimed_versions"],
                cleanup["removed_directories"],
                cleanup["archived_jobs"],
            ),
            (1, 2, 1),
        )
        self.assertNotIn("private-id", json.dumps(cleanup))
        source["repositories"][0]["enabled"] = False
        report.write_text(json.dumps(source))
        cleanup = self.collect()["repositories"][0]["cleanup"]
        self.assertEqual(
            (
                cleanup["status"],
                cleanup["claimed_versions"],
                cleanup["removed_directories"],
            ),
            ("disabled", 0, 0),
        )
        report.write_text("{")
        self.assertEqual(
            self.collect()["repositories"][0]["cleanup"]["status"], "unavailable"
        )

    def test_per_repository_cleanup_persists_when_global_run_targets_another_key(self):
        folder = self.data / "publisher/retention-results"
        folder.mkdir()
        source = dict(
            schema_version=1,
            key="assets",
            status="complete",
            apply=True,
            completed_at=time.time(),
            claimed_versions=2,
            removed_directories=3,
            archived_jobs=4,
            secret="must-not-export",
        )
        path = folder / "assets.json"
        path.write_text(json.dumps(source))
        (self.data / "publisher/retention-last.json").write_text(
            json.dumps(
                dict(
                    status="complete",
                    apply=True,
                    completed_at=time.time(),
                    repositories=[],
                )
            )
        )
        result = self.collect(True)["repositories"][0]["cleanup"]
        self.assertEqual(result["status"], "complete")
        self.assertEqual(result["claimed_versions"], 2)
        self.assertNotIn("must-not-export", json.dumps(result))
        source["key"] = "other"
        path.write_text(json.dumps(source))
        self.assertEqual(
            self.collect()["repositories"][0]["cleanup"]["status"], "unavailable"
        )

    def test_cleanup_service_does_not_expose_systemctl_output(self):
        for output, expected in [
            (b"LoadState=not-found\n", "not-installed"),
            (
                b"LoadState=loaded\nActiveState=failed\nResult=exit-code\nPRIVATE=hidden\n",
                "failed",
            ),
            (
                b"LoadState=loaded\nActiveState=inactive\nResult=success\nExecMainExitTimestampMonotonic=10\n",
                "success",
            ),
            (b"ActiveState=activating\n", "running"),
        ]:
            with patch.object(s.subprocess, "check_output", return_value=output):
                self.assertEqual(s.cleanup_service(), dict(status=expected))

    def test_filesystem_dedup_and_image_reserve_use_image_mount(self):
        def filesystem(path, *args, **kwargs):
            return SimpleNamespace(st_dev=2 if str(path) == "/data/images" else 1)

        with (
            patch.object(telemetry.os, "stat", side_effect=filesystem),
            patch.object(
                telemetry, "disk", side_effect=lambda path, label: dict(label=label)
            ),
        ):
            disks, image_index = telemetry.filesystems(Path("/data"))
        self.assertEqual(len(disks), 2)
        self.assertEqual(disks[image_index]["label"], "图片存储")
        self.assertEqual(disks[0]["label"], "系统 / 数据（同一文件系统）")

    def test_capacity_thresholds_and_publication_reserve(self):
        disk = dict(
            label="data", used_percent=20, inode_used_percent=20, available=20 * 1024**3
        )
        storage = self.collect(True)
        self.assertEqual(s.capacity([disk], storage, {})["level"], "ok")
        warn = dict(disk, used_percent=85)
        self.assertEqual(s.capacity([warn], storage, {})["level"], "warning")
        critical = dict(disk, inode_used_percent=95, available=1024**3)
        result = s.capacity([critical], storage, {})
        self.assertEqual(result["level"], "critical")
        self.assertIn("publication-reserve", [a["code"] for a in result["alerts"]])
        self.assertIn("inode-critical", [a["code"] for a in result["alerts"]])
        unknown = s.capacity([dict(disk, inode_used_percent=None)], storage, {})
        self.assertEqual(unknown["level"], "warning")
        self.assertEqual(
            s.capacity([disk], storage, dict(INFO_DISK_WARN_PERCENT="invalid"))[
                "level"
            ],
            "unknown",
        )
        unavailable = copy.deepcopy(storage)
        unavailable["status"] = "unavailable"
        self.assertNotEqual(s.capacity([disk], unavailable, {})["level"], "ok")


if __name__ == "__main__":
    unittest.main()
