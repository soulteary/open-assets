"""Manual review is specific, immutable, bounded, and invalidated by evidence."""

import fcntl
import io
import json
import time
import unittest
from unittest.mock import patch

from open_assets import closure_ack as ack, operator, reminder_info
from open_assets.object_retention import bucket_scope
import test_operator as fixture


class ClosureAcknowledgment(unittest.TestCase):
    def setUp(self):
        self.f = fixture.Operator()
        self.f.setUp()
        self.addCleanup(self.f.doCleanups)
        self.root = self.f.root
        self.data = self.root / "data"
        self.state = self.data / "publisher"
        self.state.mkdir(parents=True)
        (self.state / "worker.lock").touch()
        (self.root / ".env").write_text("DATA_ROOT=" + str(self.data) + "\n")
        self.audit_id = "1" * 32
        self.sha = "a" * 40
        self.seed(self.audit_id)
        self.base = self.data / "closure-acknowledgments/assets"

    def seed(self, audit_id, status="refused", started=None):
        now = time.time() - 1 if started is None else started
        folder = self.state / "restore-closures/assets" / audit_id
        folder.mkdir(parents=True)
        intent = dict(
            schema_version=1,
            key="assets",
            audit_id=audit_id,
            sha=self.sha,
            status="running",
            started_at=now,
            scope_digest=bucket_scope(self.f.config),
        )
        result = dict(
            intent, status=status, reason="source-unavailable", finished_at=now + 0.1
        )
        for name, value in (("intent.json", intent), ("result.json", result)):
            (folder / name).write_text(json.dumps(value))
        return folder

    def run_ack(
        self, token=None, reason="Verified negative test; no recovery was requested"
    ):
        return ack.run(self.root, "assets", self.audit_id, reason, token)

    def preview(self):
        return self.run_ack()["evidence_token"]

    def observe(self):
        snapshot, evidence = ack.observations(self.root, self.data)
        row = snapshot["restoration_closures"]["repositories"][0]
        value = ack.summary(self.root, self.data, snapshot, evidence, row)
        row["acknowledgment"] = value
        reminder_info.annotate(snapshot, evidence)
        return value, row

    def protected(self):
        return {
            str(p): p.read_bytes()
            for p in self.state.rglob("*")
            if p.is_file() and "closure-acknowledgments" not in p.parts
        }

    def test_preview_and_changed_token_never_write_acknowledgments(self):
        before = self.protected()
        value = self.run_ack()
        self.assertEqual(value["status"], "preview")
        self.assertEqual(len(value["evidence_token"]), 64)
        self.assertFalse(self.base.exists())
        self.assertEqual(self.run_ack("0" * 64)["status"], "refused")
        self.assertEqual(before, self.protected())
        self.assertFalse(self.base.exists())

    def test_confirm_is_idempotent_and_preserves_original_reports(self):
        before = self.protected()
        token = self.preview()
        result = self.run_ack(token)
        self.assertEqual(result["status"], "acknowledged")
        folder = self.base / result["ack_id"]
        self.assertEqual(
            (folder / "result.json").read_bytes(),
            (
                self.state / "restore-closures/assets" / self.audit_id / "result.json"
            ).read_bytes(),
        )
        self.assertEqual(self.run_ack(token)["ack_id"], result["ack_id"])
        self.assertTrue(self.run_ack(token)["reused"])
        self.assertEqual(len(list(self.base.glob("*/ack.json"))), 1)
        self.assertEqual(before, self.protected())
        value, row = self.observe()
        self.assertEqual(value["status"], "valid")
        self.assertEqual(
            row["reminder"], dict(state="resolved", reason="manually-acknowledged")
        )
        text = json.dumps(row)
        for private in (
            self.sha,
            self.audit_id,
            result["ack_id"],
            "negative test",
            "binding",
            "record_digest",
        ):
            self.assertNotIn(private, text)

    def test_new_attempt_invalidates_and_cannot_use_old_confirmation(self):
        token = self.preview()
        self.run_ack(token)
        self.seed("2" * 32, started=time.time())
        value, row = self.observe()
        self.assertEqual(value, dict(status="invalid", reason="new-attempt"))
        self.assertEqual(row["reminder"]["state"], "active")
        with self.assertRaises(ValueError):
            self.run_ack(token)

    def test_manifest_configuration_and_source_changes_invalidate(self):
        for mode in ("manifest", "config", "source"):
            with self.subTest(mode=mode):
                # Independent test setup avoids sharing confirmation across evidence changes.
                f = ClosureAcknowledgment()
                f.setUp()
                try:
                    token = f.preview()
                    f.run_ack(token)
                    if mode == "manifest":
                        path = f.state / "manifests/assets" / (f.sha + ".json")
                        path.parent.mkdir(parents=True)
                        path.write_text("{}")
                    elif mode == "config":
                        path = f.root / "private/repositories.yaml"
                        path.write_text(path.read_text() + "\n")
                    else:
                        path = f.state / "object-restores/assets" / (f.sha + ".json")
                        path.parent.mkdir(parents=True)
                        path.write_text(
                            json.dumps(
                                dict(
                                    schema_version=1,
                                    key="assets",
                                    sha=f.sha,
                                    status="complete",
                                    phase="complete",
                                    started_at=time.time() - 1,
                                    finished_at=time.time(),
                                    scope_digest=bucket_scope(f.f.config),
                                    files=1,
                                    verified_objects=1,
                                )
                            )
                        )
                    value, row = f.observe()
                    self.assertEqual(value["status"], "invalid")
                    self.assertEqual(row["reminder"]["state"], "active")
                    self.assertEqual(f.run_ack(token)["status"], "refused")
                finally:
                    f.doCleanups()

    def test_reference_change_invalidates_even_for_same_closure(self):
        self.run_ack(self.preview())
        path = self.data / "images/repos/assets/current"
        path.parent.mkdir(parents=True)
        path.symlink_to("releases/" + "b" * 40)
        self.assertEqual(self.observe()[0]["status"], "invalid")

    def test_corrupt_pointer_or_snapshots_fail_closed(self):
        result = self.run_ack(self.preview())
        path = self.base / result["ack_id"] / "result.json"
        path.write_text("{}")
        self.assertEqual(self.observe()[0]["status"], "unavailable")
        self.assertEqual(self.observe()[1]["reminder"]["state"], "active")
        with self.assertRaises(ValueError):
            self.run_ack(self.preview())

    def test_fixed_file_summary_never_scans_or_exposes_private_reason(self):
        self.run_ack(self.preview())
        snapshot, evidence = ack.observations(self.root, self.data)
        row = snapshot["restoration_closures"]["repositories"][0]
        before = {str(p): p.read_bytes() for p in self.base.rglob("*") if p.is_file()}
        with patch("os.scandir", side_effect=AssertionError("scan")):
            value = ack.summary(self.root, self.data, snapshot, evidence, row)
        self.assertEqual(value["status"], "valid")
        self.assertEqual(
            before,
            {str(p): p.read_bytes() for p in self.base.rglob("*") if p.is_file()},
        )
        self.assertNotIn("reason", value)

    def test_busy_limits_and_invalid_inputs_do_not_overwrite(self):
        with (self.state / "worker.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertEqual(self.run_ack()["status"], "busy")
        token = self.preview()
        self.run_ack(token)
        before = (self.base / "latest.json").read_bytes()
        with patch.object(ack, "MAX_RECORDS", 1):
            with self.assertRaises(ValueError):
                self.run_ack(token, reason="New review reason")
        self.assertEqual((self.base / "latest.json").read_bytes(), before)
        for reason in ("", "a\nb", "x" * 257):
            with self.assertRaises(ValueError):
                self.run_ack(token, reason)
        with self.assertRaises(ValueError):
            ack.run(self.root, "assets", "../bad", "reason")

    def test_pointer_failure_preserves_orphan_audit_without_activating(self):
        token = self.preview()
        with patch.object(ack.core, "atomic", side_effect=OSError("write interrupted")):
            with self.assertRaises(OSError):
                self.run_ack(token)
        self.assertEqual(self.observe()[0]["status"], "none")
        self.assertEqual(len(list(self.base.glob("*/ack.json"))), 1)
        self.assertEqual(self.run_ack(token)["status"], "acknowledged")
        self.assertEqual(len(list(self.base.glob("*/ack.json"))), 2)

    def test_cli_is_host_only_and_reason_not_echoed(self):
        args = [
            "--root",
            str(self.root),
            "acknowledge-closure",
            "assets",
            self.audit_id,
            "--reason",
            "Private review words",
        ]
        with (
            patch.object(operator.os, "geteuid", return_value=0),
            patch.object(
                operator, "inspect_command", side_effect=AssertionError("Docker")
            ),
            patch("sys.stdout", new_callable=io.StringIO) as out,
        ):
            self.assertEqual(operator.main(args), 0)
            preview = json.loads(out.getvalue())
            self.assertNotIn("Private review words", out.getvalue())
            out.seek(0)
            out.truncate()
            self.assertEqual(
                operator.main(args + ["--confirm", preview["evidence_token"]]), 0
            )
            self.assertEqual(json.loads(out.getvalue())["status"], "acknowledged")

    def test_manual_confirmation_does_not_close_uncertain_recovery_or_check(self):
        import test_reminder_info as fixture

        snapshot, evidence = fixture.sample()
        row = snapshot["restoration_closures"]["repositories"][0]
        row["attempt"]["status"] = "refused"
        row["acknowledgment"] = dict(status="valid")
        reminder_info.annotate(snapshot, evidence)
        self.assertEqual(row["reminder"]["reason"], "manually-acknowledged")
        row["attempt"].update(
            reason="ineligible", source_status="complete", source_uncertain=False
        )
        row["acknowledgment"] = dict(status="invalid", reason="evidence-changed")
        reminder_info.annotate(snapshot, evidence)
        self.assertEqual(row["reminder"]["state"], "active")
        row["acknowledgment"] = dict(status="unavailable")
        reminder_info.annotate(snapshot, evidence)
        self.assertEqual(row["reminder"]["state"], "active")
        self.assertEqual(
            snapshot["restorations"]["repositories"][0]["reminder"]["state"], "active"
        )
        self.assertEqual(
            snapshot["restoration_checks"]["repositories"][0]["reminder"]["state"],
            "active",
        )

    def test_symlink_and_unfinished_targets_cannot_be_confirmed(self):
        path = self.state / "restore-closures/assets" / self.audit_id / "result.json"
        raw = path.read_bytes()
        path.unlink()
        path.symlink_to(self.root / ".env")
        with self.assertRaises(ValueError):
            self.preview()
        path.unlink()
        path.write_bytes(raw)
        path.unlink()
        with self.assertRaises(ValueError):
            self.preview()
        self.assertFalse(self.base.exists())

    def test_evidence_change_while_recording_does_not_activate_pointer(self):
        token = self.preview()
        original = ack.observations
        calls = 0

        def observations(root, data):
            nonlocal calls
            calls += 1
            if calls == 2:
                path = self.state / "manifests/assets" / (self.sha + ".json")
                path.parent.mkdir(parents=True)
                path.write_text("{}")
            return original(root, data)

        with patch.object(ack, "observations", side_effect=observations):
            result = self.run_ack(token)
        self.assertEqual(result["status"], "refused")
        self.assertFalse((self.base / "latest.json").exists())
        self.assertEqual(len(list(self.base.glob("*/ack.json"))), 1)
        self.assertEqual(self.observe()[0]["status"], "none")

    def test_host_review_store_is_outside_worker_mount_and_rejects_writable_parents(
        self,
    ):
        self.assertNotIn(self.state, self.base.parents)
        token = self.preview()
        self.data.chmod(0o777)
        try:
            with self.assertRaises(ValueError):
                self.run_ack(token)
            self.assertFalse(self.base.exists())
        finally:
            self.data.chmod(0o755)
        self.assertEqual(self.run_ack(token)["status"], "acknowledged")
