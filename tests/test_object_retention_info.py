"""Retention observations preserve modes and never expose raw S3 metadata."""

import copy
import json
from pathlib import Path
import time
import unittest
from unittest.mock import patch

import yaml

from open_assets import object_retention as retention
from open_assets import object_retention_info as info
import test_object_retention as fixture


class RetentionInfo(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture.ObjectRetention()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.config = self.fixture.config
        self.root = self.fixture.fixture.root
        self.data = self.root / "data"
        # Reuse only disposable publishing state, but give the host its actual mapping.
        self.data.mkdir()
        (self.data / "publisher").symlink_to(
            Path(self.config["state_root"]), target_is_directory=True
        )
        (self.root / "private").mkdir()
        self.host_config = dict(
            self.config, schema_version=1, state_root="/state", images_root="/images"
        )
        self.write_config()

    def write_config(self):
        (self.root / "private/repositories.yaml").write_text(
            yaml.safe_dump(self.host_config)
        )

    def row(self, now=None):
        return info.summary(self.root, self.data, now)["repositories"][0]

    def test_preview_and_actual_execution_keep_separate_complete_records(self):
        preview = self.fixture.run_retention()
        row = self.row()
        self.assertEqual(row["attempt"]["mode"], "preview")
        self.assertNotIn("last_apply", row)
        self.assertEqual(
            row["last_preview"]["candidate_versions"], len(preview["candidates"])
        )
        self.assertEqual(
            row["last_plan"]["eligible_versions"], preview["eligible_versions"]
        )
        self.assertEqual(row["last_preview"]["removed_objects"], 0)
        self.fixture.run_retention(True)
        applied = self.row()["last_apply"]
        self.assertGreater(applied["removed_objects"], 0)
        self.fixture.run_retention()
        self.assertEqual(self.row()["last_apply"], applied)
        self.assertEqual(self.row()["attempt"]["mode"], "preview")

    def test_failure_preserves_history_and_reports_confirmed_partial_deletions(self):
        self.fixture.run_retention()
        before = self.row()["last_plan"]
        self.fixture.store.fail_after = 1
        with self.assertRaises(RuntimeError):
            self.fixture.run_retention(True)
        row = self.row()
        self.assertEqual(row["attempt"]["status"], "failed")
        self.assertTrue(row["attempt"]["partial_possible"])
        self.assertEqual(row["attempt"]["removed_objects"], 1)
        self.assertEqual(row["last_plan"], before)
        self.assertNotIn("last_apply", row)

    def test_busy_and_disabled_modes_do_not_report_actual_deletion_success(self):
        import fcntl

        self.fixture.run_retention()
        old = self.row()["last_preview"]
        with (Path(self.config["state_root"]) / "worker.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertEqual(
                retention.run(self.config, s3=self.fixture.store)["status"], "busy"
            )
        self.assertEqual(self.row()["attempt"]["reason"], "busy")
        self.assertEqual(self.row()["last_preview"], old)
        self.config["object_retention"]["enabled"] = False
        self.fixture.run_retention(True)
        self.assertEqual(self.row()["last_apply"]["reason"], "disabled")
        self.assertEqual(self.row()["last_apply"]["removed_objects"], 0)

    def test_policy_change_age_and_invalid_metadata_are_distinct_from_zero(self):
        self.assertEqual(self.row()["result_status"], "never")
        self.fixture.run_retention()
        self.assertFalse(self.row()["last_plan"]["policy_changed"])
        self.host_config["object_retention"] = dict(
            self.host_config["object_retention"], keep_days=99
        )
        self.write_config()
        self.assertTrue(self.row()["last_plan"]["policy_changed"])
        self.assertTrue(
            self.row(time.time() + info.STALE_SECONDS + 1)["last_plan"]["stale"]
        )
        path = Path(self.config["state_root"]) / "object-retention-results/assets.json"
        value = json.loads(path.read_text())
        value["last_apply"] = {"secret": "raw-object-name"}
        path.write_text(json.dumps(value))
        row = self.row()
        self.assertIn("last_apply", row["invalid_fields"])
        self.assertNotIn("raw-object-name", json.dumps(row))
        path.write_text("{broken")
        self.assertEqual(self.row()["result_status"], "unavailable")

    def test_snapshot_allowlist_filters_shas_paths_tokens_and_policy_pins(self):
        sha = self.fixture.fixture.shas[0]
        self.config["object_retention"]["pinned_shas"] = [sha]
        self.host_config["object_retention"] = copy.deepcopy(
            self.config["object_retention"]
        )
        self.write_config()
        self.fixture.run_retention()
        path = Path(self.config["state_root"]) / "object-retention-results/assets.json"
        source = json.loads(path.read_text())
        source["endpoint"] = "private-host"
        source["attempt"]["secret"] = "raw-token"
        path.write_text(json.dumps(source))
        value = json.dumps(self.row())
        for forbidden in (
            sha,
            "private-host",
            "raw-token",
            "policy_digest",
            "pinned_shas",
            "endpoint",
        ):
            self.assertNotIn(forbidden, value)
        self.assertEqual(self.row()["policy"]["pinned_count"], 1)

    def test_single_repository_and_late_completion_do_not_overwrite_other_attempts(
        self,
    ):
        self.fixture.run_retention()
        path = Path(self.config["state_root"]) / "object-retention-results/assets.json"
        before = json.loads(path.read_text())
        old = before["attempt"]["started_at"]
        info.persist(
            self.config,
            "assets",
            dict(
                mode="preview",
                started_at=old - 1,
                finished_at=time.time(),
                status="busy",
                reason="busy",
            ),
        )
        self.assertEqual(json.loads(path.read_text())["attempt"], before["attempt"])
        other = dict(before, key="other")
        info_path = path.with_name("other.json")
        info_path.write_text(json.dumps(other))
        raw = info_path.read_bytes()
        self.fixture.run_retention()
        self.assertEqual(info_path.read_bytes(), raw)

    def test_pre_delete_failure_has_no_partial_flag_and_known_reason(self):
        self.config["object_retention"]["exclusive_writer"] = False
        with self.assertRaises(ValueError):
            self.fixture.run_retention(True)
        attempt = self.row()["attempt"]
        self.assertEqual(attempt["reason"], "exclusive-writer-required")
        self.assertFalse(attempt["partial_possible"])
        self.assertEqual(attempt["removed_objects"], 0)
        error = ValueError("private unexpected text")
        self.assertEqual(info.error_reason(error), "unsafe-state")

    def test_reader_does_not_invoke_s3_and_limits_explain_excluded_versions(self):
        self.config["object_retention"]["max_delete_bytes"] = 1
        plan = self.fixture.run_retention()
        self.assertGreater(plan["skipped_versions"], 0)
        self.assertEqual(plan["candidates"], [])
        with patch(
            "open_assets.object_info.client",
            side_effect=AssertionError("S3 is forbidden"),
        ):
            row = self.row()
        self.assertGreater(row["last_plan"]["eligible_versions"], 0)
        self.assertGreater(row["last_plan"]["skip_counts"]["bytes-limit"], 0)
        self.assertEqual(row["last_plan"]["candidate_versions"], 0)

    def test_service_condition_skip_is_not_reported_as_execution_success(self):
        from open_assets.storage_info import cleanup_service

        with patch(
            "subprocess.check_output",
            return_value=b"LoadState=loaded\nConditionResult=no\nResult=success\nActiveState=inactive\nExecMainExitTimestampMonotonic=100\n",
        ):
            self.assertEqual(
                cleanup_service("open-assets-object-retention.service")["status"],
                "condition-skipped",
            )


if __name__ == "__main__":
    unittest.main()
