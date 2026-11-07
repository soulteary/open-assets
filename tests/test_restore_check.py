"""Diagnostic checks never repair data or overwrite restoration evidence."""

import copy
import io
import json
import os
import shutil
import unittest
from unittest.mock import patch

import yaml

from open_assets import restore_check as check, restore_check_info as info
import test_object_restore as fixture

core = fixture.core


class RestoreCheck(unittest.TestCase):
    def setUp(self):
        self.f = fixture.ObjectRestore()
        self.f.setUp()
        self.addCleanup(self.f.doCleanups)
        self.f.run_restore()
        self.config = self.f.config
        self.path = check.report_path(self.config, "assets", self.f.sha)
        self.source = fixture.restore.report_path(self.config, "assets", self.f.sha)

    def run_check(self):
        with patch.object(
            self.f.store, "put_object", side_effect=AssertionError("write forbidden")
        ):
            return check.run(self.config, "assets", self.f.sha, self.f.store)

    def snapshot(self):
        return {
            str(p): (
                os.readlink(p) if p.is_symlink() else p.read_bytes(),
                p.lstat().st_mtime_ns,
            )
            for root in (self.f.state, self.f.images)
            for p in root.rglob("*")
            if (p.is_file() or p.is_symlink())
            and "restore-checks" not in p.parts
            and p.name != "worker.lock"
        }

    def host(self):
        root = self.f.root / "host"
        (root / "private").mkdir(parents=True, exist_ok=True)
        (root / "data").mkdir(exist_ok=True)
        for name, target in (("publisher", self.f.state), ("images", self.f.images)):
            p = root / "data" / name
            if not p.exists():
                p.symlink_to(target, target_is_directory=True)
        (root / "private/repositories.yaml").write_text(
            yaml.safe_dump(
                dict(
                    self.config,
                    schema_version=1,
                    state_root="/state",
                    images_root="/images",
                )
            )
        )
        return root

    def row(self, now=None):
        root = self.host()
        return info.summary(root, root / "data", now)["repositories"][0]

    def test_verified_repeat_preserves_all_data_and_metadata_times(self):
        before = self.snapshot()
        for _ in range(2):
            report = self.run_check()
            self.assertEqual(report["diagnosis"], "verified")
            self.assertTrue(all(c["status"] == "ok" for c in report["checks"].values()))
            self.assertFalse(
                report["current_is_target"] or report["published_is_target"]
            )
            self.assertEqual(self.snapshot(), before)

    def test_unresolved_source_is_diagnosed_but_not_closed_or_activated(self):
        original = json.loads(self.source.read_text())
        original.update(
            status="running",
            phase="activate",
            activation_uncertain=True,
            rollback_requested=True,
            with_local=True,
            local_verified=True,
        )
        self.source.write_text(json.dumps(original))
        before = self.snapshot()
        report = self.run_check()
        self.assertEqual(report["source_status"], "running")
        self.assertTrue(report["source_uncertain"])
        self.assertEqual(report["diagnosis"], "verified")
        self.assertFalse(report["current_is_target"])
        self.assertEqual(self.snapshot(), before)

    def test_missing_objects_and_local_copies_do_not_hide_reference_result(self):
        self.f.clear_objects()
        self.f.remove_local()
        report = self.run_check()
        self.assertEqual(report["diagnosis"], "issues")
        for name in ("objects", "full", "images"):
            self.assertEqual(report["checks"][name]["status"], "failed")
        self.assertEqual(report["checks"]["references"]["status"], "ok")

    def test_object_bytes_metadata_extra_and_local_link_conflicts(self):
        original = copy.deepcopy(self.f.store.objects)
        key = self.f.prefix() + "images/pic.png"
        for field, value in (
            ("data", b"corrupt!!"),
            ("mime", "text/plain"),
            ("cache", "no-cache"),
        ):
            self.f.store.objects = copy.deepcopy(original)
            self.f.store.objects[key][field] = value
            self.assertEqual(self.run_check()["checks"]["objects"]["status"], "failed")
        self.f.store.objects = copy.deepcopy(original)
        self.f.store.objects[self.f.prefix() + "unexpected"] = copy.deepcopy(
            original[key]
        )
        self.assertEqual(self.run_check()["checks"]["objects"]["status"], "failed")
        image = self.f.images / "repos/assets/releases" / ("images-" + self.f.sha)
        shutil.rmtree(image)
        image.symlink_to(
            self.f.images / "repos/assets/releases" / ("images-" + self.f.current)
        )
        self.assertEqual(self.run_check()["checks"]["images"]["status"], "failed")

    def test_reference_mismatch_and_regular_current_are_never_repaired(self):
        published = self.f.state / "published/assets"
        published.write_text(json.dumps(self.f.manifests[0]))
        before = self.snapshot()
        report = self.run_check()
        self.assertEqual(report["checks"]["references"]["status"], "failed")
        self.assertEqual(before, self.snapshot())
        current = self.f.images / "repos/assets/current"
        current.unlink()
        current.write_text("keep")
        self.assertEqual(self.run_check()["checks"]["references"]["status"], "failed")
        self.assertEqual(current.read_text(), "keep")

    def test_barrier_remains_blocked_even_if_every_byte_is_present(self):
        value = json.loads(self.f.marker.read_text())
        value["status"] = "complete"
        self.f.marker.write_text(json.dumps(value))
        before = self.f.marker.read_bytes()
        report = self.run_check()
        self.assertEqual(report["checks"]["objects"]["status"], "ok")
        self.assertEqual(report["checks"]["barrier"]["status"], "blocked")
        self.assertEqual(report["diagnosis"], "issues")
        self.assertEqual(self.f.marker.read_bytes(), before)

    def test_missing_manifest_source_corruption_and_timeout_are_unresolved(self):
        self.source.write_text("{")
        self.assertEqual(self.run_check()["diagnosis"], "issues")
        with patch.object(
            fixture.restore.Budget, "check", side_effect=TimeoutError("private URL")
        ):
            report = self.run_check()
        self.assertNotIn("private URL", json.dumps(report))
        self.assertTrue(all(c["status"] == "failed" for c in report["checks"].values()))
        (self.f.state / "manifests/assets" / (self.f.sha + ".json")).unlink()
        self.assertEqual(self.run_check()["status"], "failed")

    def test_busy_and_invalid_identity_create_no_report(self):
        import fcntl

        with (self.f.state / "worker.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertEqual(self.run_check()["status"], "busy")
        self.assertFalse(self.path.exists())
        with self.assertRaises(ValueError):
            check.run(self.config, "../bad", self.f.sha, self.f.store)

    def test_cli_success_issues_busy_and_restricted_flags(self):
        for report, code in (
            (dict(status="complete", diagnosis="verified"), None),
            (dict(status="complete", diagnosis="issues"), 1),
            (dict(status="busy"), 2),
        ):
            output = io.StringIO()
            with (
                patch.object(core, "load", return_value=self.config),
                patch.object(check, "run", return_value=report),
                patch(
                    "sys.argv", ["open_assets", "check-restore", "assets", self.f.sha]
                ),
                patch("sys.stdout", output),
            ):
                if code is None:
                    core.main()
                else:
                    with self.assertRaises(SystemExit) as error:
                        core.main()
                    self.assertEqual(error.exception.code, code)
        with (
            patch(
                "sys.argv",
                ["open_assets", "check-restore", "assets", self.f.sha, "--rollback"],
            ),
            patch("sys.stderr", io.StringIO()),
        ):
            with self.assertRaises(SystemExit) as error:
                core.main()
            self.assertEqual(error.exception.code, 2)

    def test_summary_filtered_read_only_and_changes_expiry_are_explicit(self):
        self.run_check()
        before = self.snapshot()
        row = self.row()
        a = row["attempt"]
        self.assertEqual(row["result_status"], "ok")
        self.assertFalse(
            a.get("references_changed")
            or a.get("source_changed")
            or a.get("manifest_changed")
            or a.get("observation_unavailable")
        )
        self.assertNotIn(self.f.sha, json.dumps(row))
        self.assertNotIn("scope_digest", json.dumps(row))
        self.assertEqual(before, self.snapshot())
        self.assertTrue(self.row(a["finished_at"] + 86401)["attempt"]["stale"])
        value = json.loads(self.source.read_text())
        value["uploaded_objects"] = 0
        self.source.write_text(json.dumps(value))
        self.assertTrue(self.row()["attempt"]["source_changed"])
        core.rollback_images(self.config, "assets", self.f.sha)
        self.assertTrue(self.row()["attempt"]["references_changed"])
        manifest = self.f.state / "manifests/assets" / (self.f.sha + ".json")
        v = json.loads(manifest.read_text())
        v["config"] = "changed"
        manifest.write_text(json.dumps(v))
        self.assertTrue(self.row()["attempt"]["manifest_changed"])

    def test_summary_corruption_limits_links_and_running_never_green(self):
        self.assertEqual(self.row()["result_status"], "never")
        self.run_check()
        with patch.object(info, "MAX_REPORTS", 0):
            self.assertEqual(self.row()["result_status"], "unavailable")
        with patch.object(info, "SCAN_SECONDS", -1):
            self.assertEqual(self.row()["result_status"], "unavailable")
        value = json.loads(self.path.read_text())
        value.update(status="running", diagnosis="unresolved", phase="objects")
        value.pop("finished_at")
        self.path.write_text(json.dumps(value))
        self.assertEqual(self.row()["attempt"]["status"], "running")
        corrupt = self.path.with_name("b" * 40 + ".json")
        corrupt.write_text("{")
        self.assertEqual(self.row()["result_status"], "unavailable")
        corrupt.unlink()
        corrupt.symlink_to(self.path)
        self.assertEqual(self.row()["result_status"], "unavailable")

    def test_target_current_reports_present_selection_not_past_success(self):
        core.rollback_images(self.config, "assets", self.f.sha)
        report = self.run_check()
        self.assertTrue(report["current_is_target"] and report["published_is_target"])
        self.assertEqual(report["checks"]["references"]["reason"], "target-current")
        self.assertNotIn("activated", report)
