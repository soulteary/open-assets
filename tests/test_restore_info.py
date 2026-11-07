"""Restoration summaries are bounded, filtered and read-only."""

import copy
import json
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

import yaml

from open_assets import restore_info as info
from open_assets.object_retention import bucket_scope


class RestoreInfo(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "private").mkdir()
        self.config = dict(
            schema_version=1,
            endpoint="https://private.invalid",
            bucket="attachments",
            state_root="/state",
            repositories=[dict(key="assets"), dict(key="other")],
        )
        self.write_config()
        self.folder = self.root / "data/publisher/object-restores/assets"
        self.folder.mkdir(parents=True)
        self.now = time.time() - 10
        self.report = dict(
            schema_version=1,
            key="assets",
            sha="a" * 40,
            status="complete",
            phase="complete",
            started_at=self.now - 5,
            finished_at=self.now,
            scope_digest=bucket_scope(self.config),
            files=2,
            verified_objects=2,
            uploaded_objects=2,
            repaired_objects=0,
            with_local=True,
            rollback_requested=False,
            local_verified=True,
            local_full_rebuilt=True,
            local_images_rebuilt=True,
            partial_possible=False,
            local_partial_possible=False,
            activated=False,
            activation_uncertain=False,
        )

    def write_config(self):
        (self.root / "private/repositories.yaml").write_text(
            yaml.safe_dump(self.config)
        )

    def write(self, value=None, sha="a" * 40):
        v = copy.deepcopy(self.report if value is None else value)
        v["sha"] = sha
        (self.folder / (sha + ".json")).write_text(json.dumps(v))

    def summary(self):
        return info.summary(self.root, self.root / "data")

    def row(self):
        return self.summary()["repositories"][0]

    def test_complete_local_default_filtered_read_only(self):
        self.report.update(
            error="secret request URL", endpoint="secret", paths=["secret"]
        )
        self.write()
        before = {p: p.read_bytes() for p in self.folder.iterdir()}
        a = self.row()["attempt"]
        self.assertTrue(a["objects_verified"] and a["local_verified"])
        self.assertFalse(a["activated"] or a["uncertain"])
        self.assertNotIn("secret", json.dumps(self.summary()))
        self.assertNotIn("sha", a)
        self.assertEqual(before, {p: p.read_bytes() for p in self.folder.iterdir()})
        self.assertEqual(self.summary()["repositories"][1]["result_status"], "never")

    def test_latest_start_wins_not_mtime_or_completion_time(self):
        self.write()
        newer = dict(
            self.report,
            started_at=self.now - 1,
            status="failed",
            phase="git",
            finished_at=self.now,
        )
        self.write(newer, "b" * 40)
        self.assertEqual(self.row()["attempt"]["status"], "failed")

    def test_partial_failure_keeps_remote_verification_and_uncertainty(self):
        self.write(
            dict(
                self.report,
                status="failed",
                phase="local-install",
                local_verified=False,
                local_partial_possible=True,
            )
        )
        a = self.row()["attempt"]
        self.assertTrue(a["objects_verified"] and a["uncertain"])
        self.assertFalse(a["local_verified"])

    def test_explicit_activation_and_uncertain_activation(self):
        self.write(dict(self.report, rollback_requested=True, activated=True))
        self.assertTrue(self.row()["attempt"]["activated"])
        self.write(
            dict(
                self.report,
                rollback_requested=True,
                status="failed",
                phase="activate",
                activation_uncertain=True,
            )
        )
        self.assertTrue(self.row()["attempt"]["uncertain"])

    def test_running_is_unresolved_without_guessing_process_liveness(self):
        v = dict(self.report, status="running", phase="verify")
        v.pop("finished_at")
        self.write(v)
        self.assertTrue(self.row()["attempt"]["uncertain"])
        self.assertFalse(self.row()["attempt"]["objects_verified"])

    def test_bad_reports_never_fall_back_to_older_success(self):
        self.write()
        path = self.folder / ("b" * 40 + ".json")
        for payload in (
            "{",
            json.dumps(dict(self.report, sha="b" * 40, verified_objects=True)),
            "x" * 16385,
        ):
            path.write_text(payload)
            self.assertEqual(self.row()["result_status"], "unavailable")
        path.unlink()
        path.symlink_to(self.folder / ("a" * 40 + ".json"))
        self.assertEqual(self.row()["result_status"], "unavailable")

    def test_limits_and_directory_links_fail_closed(self):
        self.write()
        with patch.object(info, "MAX_REPORTS", 0):
            self.assertEqual(self.row()["result_status"], "unavailable")
        with patch.object(info, "SCAN_SECONDS", -1):
            self.assertEqual(self.row()["result_status"], "unavailable")
        self.folder.rename(self.folder.with_name("hidden"))
        self.folder.symlink_to(
            self.folder.with_name("hidden"), target_is_directory=True
        )
        self.assertEqual(self.row()["result_status"], "unavailable")

    def test_scope_changes_and_legacy_missing_flags_remain_visible(self):
        v = {k: v for k, v in self.report.items() if k not in info.FLAGS}
        self.write(v)
        self.config["bucket"] = "changed-bucket"
        self.write_config()
        a = self.row()["attempt"]
        self.assertTrue(a["scope_changed"])
        self.assertNotIn("rollback_requested", a)

    def test_atomic_temporary_is_ignored_but_unknown_entry_is_not(self):
        self.write()
        (self.folder / "tmpabcdefgh").write_text("{")
        self.assertEqual(self.row()["result_status"], "ok")
        (self.folder / "unknown").write_text("{}")
        self.assertEqual(self.row()["result_status"], "unavailable")

    def test_invalid_host_mapping_or_config_is_unavailable(self):
        self.config["state_root"] = "/unexpected"
        self.write_config()
        self.assertEqual(self.summary()["status"], "unavailable")
