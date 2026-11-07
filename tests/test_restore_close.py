"""Closure requires fresh verification and preserves source/audit evidence."""

import io
import json
import unittest
from unittest.mock import patch

from open_assets import (
    restore_check as check,
    restore_close as close,
    restore_close_info as info,
)
import test_restore_check as fixture

core = fixture.core


class RestoreClose(unittest.TestCase):
    def setUp(self):
        self.helper = fixture.RestoreCheck()
        self.helper.setUp()
        self.addCleanup(self.helper.doCleanups)
        self.f = self.helper.f
        self.config = self.f.config
        self.source = self.helper.source
        with patch.object(
            core,
            "rollback_images_locked",
            side_effect=OSError("fixture activation interrupted"),
        ):
            with self.assertRaises(OSError):
                fixture.fixture.restore.run(
                    self.config,
                    "assets",
                    self.f.sha,
                    self.f.store,
                    with_local=True,
                    rollback=True,
                )
        self.original = self.source.read_bytes()
        self.parent = self.f.state / "restore-closures/assets"

    def run_close(self):
        with patch.object(
            self.f.store, "put_object", side_effect=AssertionError("write forbidden")
        ):
            return close.run(
                self.config,
                "assets",
                self.f.sha,
                "Operator reviewed current serving version",
                self.f.store,
            )

    def snapshot(self):
        return {
            p: v
            for p, v in self.helper.snapshot().items()
            if "/restore-closures/" not in p
        }

    def directory(self, result):
        return self.parent / result["audit_id"]

    def row(self):
        root = self.helper.host()
        return info.summary(root, root / "data")["repositories"][0]

    def test_closed_after_fresh_check_preserves_source_data_and_times(self):
        before = self.snapshot()
        result = self.run_close()
        self.assertEqual(result["status"], "closed")
        self.assertFalse(result["current_is_target"])
        self.assertEqual(self.source.read_bytes(), self.original)
        self.assertEqual(before, self.snapshot())
        directory = self.directory(result)
        self.assertEqual((directory / "source.json").read_bytes(), self.original)
        checked = json.loads((directory / "check.json").read_text())
        self.assertEqual(checked["diagnosis"], "verified")
        self.assertGreaterEqual(checked["started_at"], result["started_at"])
        self.assertTrue(all(c["status"] == "ok" for c in checked["checks"].values()))
        self.assertEqual(directory.stat().st_mode & 0o777, 0o700)
        self.assertEqual((directory / "source.json").stat().st_mode & 0o777, 0o600)
        row = self.row()
        self.assertEqual(row["attempt"]["status"], "closed")
        self.assertFalse(row["attempt"].get("observation_unavailable"))

    def test_repeated_closure_adds_audit_without_replacing_any_old_evidence(self):
        first = self.run_close()
        directory = self.directory(first)
        before = {p.name: p.read_bytes() for p in directory.iterdir()}
        second = self.run_close()
        self.assertEqual(second["status"], "closed")
        self.assertNotEqual(first["audit_id"], second["audit_id"])
        self.assertEqual(before, {p.name: p.read_bytes() for p in directory.iterdir()})
        self.assertEqual(self.source.read_bytes(), self.original)

    def test_running_report_can_close_but_is_never_rewritten(self):
        value = json.loads(self.original)
        value.update(status="running")
        value.pop("finished_at")
        self.source.write_text(json.dumps(value))
        before = self.source.read_bytes()
        result = self.run_close()
        self.assertEqual(result["status"], "closed")
        self.assertEqual(self.source.read_bytes(), before)

    def test_ineligible_missing_and_corrupt_sources_are_audited_refusals(self):
        value = json.loads(self.original)
        value.update(
            status="complete",
            phase="complete",
            activation_uncertain=False,
            partial_possible=False,
        )
        self.source.write_text(json.dumps(value))
        result = self.run_close()
        self.assertEqual(result["reason"], "ineligible")
        self.assertFalse((self.directory(result) / "check.json").exists())
        for payload in ("{", None):
            if payload is None:
                self.source.unlink()
            else:
                self.source.write_text(payload)
            result = self.run_close()
            self.assertEqual(result["status"], "refused")
            self.assertEqual(result["reason"], "source-unavailable")

    def test_cached_success_does_not_authorize_closure_after_object_loss(self):
        self.assertEqual(self.helper.run_check()["diagnosis"], "verified")
        self.f.clear_objects()
        before = self.snapshot()
        result = self.run_close()
        self.assertEqual(result["status"], "refused")
        self.assertEqual(result["reason"], "check-failed")
        self.assertEqual(before, self.snapshot())
        self.assertEqual(self.source.read_bytes(), self.original)

    def test_missing_local_copy_refuses_without_rebuilding(self):
        self.f.remove_local()
        result = self.run_close()
        self.assertEqual(result["status"], "refused")
        self.assertFalse(
            (self.f.images / "repos/assets/releases" / self.f.sha).exists()
        )

    def test_inconsistent_references_are_not_repaired_or_closed(self):
        path = self.f.state / "published/assets"
        path.write_text(json.dumps(self.f.manifests[0]))
        before = self.snapshot()
        self.assertEqual(self.run_close()["status"], "refused")
        self.assertEqual(before, self.snapshot())

    def test_barrier_prevents_closure_without_being_cleared(self):
        value = json.loads(self.f.marker.read_text())
        value["status"] = "complete"
        self.f.marker.write_text(json.dumps(value))
        before = self.f.marker.read_bytes()
        result = self.run_close()
        self.assertEqual(result["status"], "refused")
        self.assertEqual(self.f.marker.read_bytes(), before)
        self.assertEqual(
            json.loads((self.directory(result) / "check.json").read_text())["checks"][
                "barrier"
            ]["status"],
            "blocked",
        )

    def test_unpublished_references_are_not_enough_to_close(self):
        (self.f.images / "repos/assets/current").unlink()
        (self.f.state / "published/assets").unlink()
        result = self.run_close()
        self.assertEqual(result["status"], "refused")
        self.assertEqual(result["reason"], "references-missing")

    def test_changed_source_during_verification_prevents_closure(self):
        real = check.run_locked

        def changed(*args, **kwargs):
            checked = real(*args, **kwargs)
            value = json.loads(self.source.read_text())
            value["uploaded_objects"] += 1
            self.source.write_text(json.dumps(value))
            return checked

        with patch.object(check, "run_locked", side_effect=changed):
            result = self.run_close()
        self.assertEqual(result["status"], "refused")
        self.assertEqual(result["reason"], "evidence-changed")
        self.assertEqual(
            (self.directory(result) / "source.json").read_bytes(), self.original
        )

    def test_interrupted_final_record_is_unclosed_and_retry_preserves_it(self):
        real = close.save

        def interrupted(path, value):
            if path.name == "result.json":
                raise OSError("fixture final write interruption")
            return real(path, value)

        with patch.object(close, "save", side_effect=interrupted):
            with self.assertRaises(OSError):
                self.run_close()
        directory = next(self.parent.iterdir())
        before = {p.name: p.read_bytes() for p in directory.iterdir()}
        self.assertEqual(self.row()["attempt"]["status"], "running")
        self.assertEqual(self.run_close()["status"], "closed")
        self.assertEqual(before, {p.name: p.read_bytes() for p in directory.iterdir()})

    def test_write_once_never_replaces_existing_audit_and_tamper_is_not_green(self):
        result = self.run_close()
        directory = self.directory(result)
        with self.assertRaises(FileExistsError):
            close.write_once(directory / "source.json", b"replacement")
        self.assertEqual((directory / "source.json").read_bytes(), self.original)
        (directory / "check.json").write_text("{}")
        self.assertEqual(self.row()["result_status"], "unavailable")

    def test_summary_filtering_changes_limits_and_links(self):
        result = self.run_close()
        row = self.row()
        text = json.dumps(row)
        for private in (
            "Operator reviewed",
            self.f.sha,
            result["audit_id"],
            "scope_digest",
            "execution_uid",
        ):
            self.assertNotIn(private, text)
        with patch.object(info, "MAX_ATTEMPTS", 0):
            self.assertEqual(self.row()["result_status"], "unavailable")
        with patch.object(info, "SCAN_SECONDS", -1):
            self.assertEqual(self.row()["result_status"], "unavailable")
        value = json.loads(self.source.read_text())
        value["uploaded_objects"] += 1
        self.source.write_text(json.dumps(value))
        self.assertTrue(self.row()["attempt"]["source_changed"])
        core.rollback_images(self.config, "assets", self.f.sha)
        self.assertTrue(self.row()["attempt"]["references_changed"])
        directory = self.directory(result)
        directory.rename(directory.with_name("hidden"))
        directory.symlink_to(directory.with_name("hidden"), target_is_directory=True)
        self.assertEqual(self.row()["result_status"], "unavailable")

    def test_busy_invalid_reason_and_cli_contract(self):
        import fcntl

        with (self.f.state / "worker.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertEqual(self.run_close()["status"], "busy")
        self.assertFalse(self.parent.exists())
        for reason in ("", "x" * 257, "line\nvalue"):
            with self.assertRaises(ValueError):
                close.run(self.config, "assets", self.f.sha, reason, self.f.store)
        for report, exit_code in (
            (dict(status="closed"), None),
            (dict(status="refused"), 1),
            (dict(status="busy"), 2),
        ):
            with (
                patch.object(core, "load", return_value=self.config),
                patch.object(close, "run", return_value=report),
                patch(
                    "sys.argv",
                    [
                        "open_assets",
                        "close-restore",
                        "assets",
                        self.f.sha,
                        "--reason",
                        "reviewed",
                    ],
                ),
                patch("sys.stdout", io.StringIO()),
            ):
                if exit_code is None:
                    core.main()
                else:
                    with self.assertRaises(SystemExit) as stopped:
                        core.main()
                    self.assertEqual(stopped.exception.code, exit_code)
        for argv in (
            ["close-restore", "assets", self.f.sha],
            ["check-restore", "assets", self.f.sha, "--reason", "reviewed"],
            [
                "close-restore",
                "assets",
                self.f.sha,
                "--reason",
                "reviewed",
                "--rollback",
            ],
        ):
            with (
                patch("sys.argv", ["open_assets", *argv]),
                patch("sys.stderr", io.StringIO()),
            ):
                with self.assertRaises(SystemExit) as stopped:
                    core.main()
                self.assertEqual(stopped.exception.code, 2)
