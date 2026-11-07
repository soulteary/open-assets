"""Archive validation must precede online retirement and survives partial cleanup."""

import gzip
import io
import json
from pathlib import Path
import tarfile
import time
import unittest
from unittest.mock import patch
import uuid

from open_assets import (
    restore_audit as audit,
)
import test_restore_close as fixture

core = fixture.core


class RestoreAudit(unittest.TestCase):
    def setUp(self):
        self.helper = fixture.RestoreClose()
        self.helper.setUp()
        self.addCleanup(self.helper.doCleanups)
        self.f = self.helper.f
        self.config = self.f.config
        self.first = self.helper.run_close()
        self.second = self.helper.run_close()
        self.paths = audit.directories(self.config, "assets")

    def run_audit(self, **kwargs):
        return audit.run(self.config, "assets", **kwargs)

    def archive(self, apply=False, **kwargs):
        return self.run_audit(
            action="archive", apply=apply, keep_last=1, min_age_days=0, **kwargs
        )

    def snapshot(self):
        return {str(p): p.read_bytes() for p in self.f.state.rglob("*") if p.is_file()}

    def terminal(self, sha=None, started=None, result=True, **fields):
        audit_id = uuid.uuid4().hex
        directory = self.paths["online"] / audit_id
        directory.mkdir()
        start = started if started is not None else time.time() - 1000
        intent = dict(
            schema_version=1,
            key="assets",
            sha=sha or self.f.sha,
            audit_id=audit_id,
            status="running",
            started_at=start,
            scope_digest=self.first["scope_digest"],
            reason="private operator note",
            execution_uid=20010,
        )
        (directory / "intent.json").write_text(json.dumps(intent))
        if result:
            value = {
                k: intent[k]
                for k in (
                    "schema_version",
                    "key",
                    "sha",
                    "audit_id",
                    "started_at",
                    "scope_digest",
                )
            }
            value.update(
                status="refused",
                reason="source-unavailable",
                finished_at=start + 1,
                **fields,
            )
            (directory / "result.json").write_text(json.dumps(value))
        return audit_id

    def test_paginated_query_filters_sha_includes_archives_without_private_notes(self):
        other = self.terminal(sha=self.f.current)
        result = self.run_audit(limit=1)
        self.assertEqual(result["total"], 3)
        self.assertTrue(result["has_more"])
        self.assertEqual(self.run_audit(offset=1, limit=2)["total"], 3)
        self.assertEqual(
            self.run_audit(sha=self.f.current)["records"][0]["audit_id"], other
        )
        self.assertNotIn("private operator", json.dumps(result))
        self.archive(apply=True)
        self.assertEqual(self.run_audit(sha=self.f.sha)["total"], 2)
        self.assertTrue(
            any("archive" in r["locations"] for r in self.run_audit()["records"])
        )

    def test_preview_creates_nothing_and_defaults_keep_recent_records(self):
        before = self.snapshot()
        self.assertEqual(self.run_audit(action="archive")["candidates"], [])
        result = self.archive()
        self.assertEqual(len(result["candidates"]), 1)
        self.assertEqual(before, self.snapshot())
        self.assertFalse(
            self.paths["archive"].exists() or self.paths["retired"].exists()
        )

    def test_apply_full_byte_roundtrip_repeat_and_latest_online_protection(self):
        directory = self.paths["online"] / self.first["audit_id"]
        original = audit.read_directory(directory)
        result = self.archive(apply=True)
        self.assertEqual(result["archived_records"], 1)
        self.assertFalse(directory.exists())
        self.assertTrue((self.paths["online"] / self.second["audit_id"]).exists())
        _, saved = audit.verify_archive(
            self.paths["archive"] / (self.first["audit_id"] + ".tar.gz"),
            "assets",
            self.first["audit_id"],
        )
        self.assertEqual(original, saved)
        self.assertEqual(self.archive(apply=True)["archived_records"], 0)
        self.assertEqual(self.run_audit(action="verify")["status"], "complete")
        self.assertEqual(self.helper.row()["attempt"]["status"], "closed")

    def test_unfinished_invalid_and_unclosed_source_attempts_are_protected(self):
        running = self.terminal(result=False)
        invalid = self.terminal()
        (self.paths["online"] / invalid / "unexpected").write_text("keep")
        value = json.loads(self.helper.original)
        value["uploaded_objects"] += 1
        self.helper.source.write_text(json.dumps(value))
        # A refusal concerning this new uncertain source has no matching closed audit.
        self.f.clear_objects()
        refused = self.helper.run_close()
        self.assertEqual(refused["status"], "refused")
        latest = self.terminal(started=time.time() + 1)
        result = self.archive()
        protected = {r["audit_id"]: r["protection"] for r in result["protected"]}
        self.assertEqual(protected[running], "unfinished")
        self.assertEqual(protected[invalid], "invalid")
        self.assertEqual(protected[refused["audit_id"]], "unresolved-source")
        self.assertTrue((self.paths["online"] / latest).exists())
        self.archive(apply=True)
        self.assertTrue((self.paths["online"] / running).exists())
        self.assertTrue((self.paths["online"] / refused["audit_id"]).exists())

    def test_corrupt_closed_evidence_is_not_archived(self):
        directory = self.paths["online"] / self.first["audit_id"]
        (directory / "source.json").write_text("{}")
        result = self.archive(apply=True)
        self.assertEqual(result["archived_records"], 0)
        self.assertEqual(result["invalid_records"], 1)
        self.assertTrue(directory.exists())

    def test_existing_archive_conflict_never_overwrites_or_removes_online(self):
        self.paths["archive"].parent.mkdir()
        self.paths["archive"].mkdir()
        target = self.paths["archive"] / (self.first["audit_id"] + ".tar.gz")
        target.write_bytes(b"corrupt")
        before = (
            self.paths["online"] / self.first["audit_id"] / "source.json"
        ).read_bytes()
        result = self.archive(apply=True)
        self.assertEqual(result["archived_records"], 0)
        self.assertEqual(target.read_bytes(), b"corrupt")
        self.assertEqual(
            (
                self.paths["online"] / self.first["audit_id"] / "source.json"
            ).read_bytes(),
            before,
        )
        self.assertEqual(self.run_audit(action="verify")["status"], "failed")
        self.assertEqual(
            self.run_audit(action="verify", sha=self.f.current)["status"], "failed"
        )

    def test_failure_after_archive_publish_reuses_verified_package(self):
        with patch.object(
            audit.os, "rename", side_effect=OSError("fixture interruption")
        ):
            result = self.archive(apply=True)
        self.assertEqual(result["status"], "failed")
        self.assertTrue(result["partial_possible"])
        package = self.paths["archive"] / (self.first["audit_id"] + ".tar.gz")
        before = package.read_bytes()
        self.assertTrue((self.paths["online"] / self.first["audit_id"]).exists())
        self.assertEqual(self.archive(apply=True)["archived_records"], 1)
        self.assertEqual(package.read_bytes(), before)

    def interrupt_retirement(self):
        original = Path.unlink
        calls = [0]

        def interrupted(path, *args, **kwargs):
            if "restore-audit-retired" in path.parts:
                calls[0] += 1
                if calls[0] == 2:
                    raise OSError("fixture partial unlink")
            return original(path, *args, **kwargs)

        with patch.object(Path, "unlink", interrupted):
            result = self.archive(apply=True)
        self.assertEqual(result["status"], "failed")
        return self.paths["retired"] / self.first["audit_id"]

    def test_partial_retirement_resumes_from_archive_without_losing_bytes(self):
        retired = self.interrupt_retirement()
        self.assertTrue(retired.exists())
        self.assertEqual(self.run_audit(action="verify")["status"], "complete")
        self.assertEqual(self.archive(apply=True)["archived_records"], 1)
        self.assertFalse(retired.exists())

    def test_corrupt_archive_or_conflicting_retired_bytes_stop_resumption(self):
        retired = self.interrupt_retirement()
        remaining = next(retired.iterdir())
        remaining.write_bytes(b"conflict")
        before = self.snapshot()
        self.assertEqual(self.archive(apply=True)["archived_records"], 0)
        self.assertEqual(
            {p: v for p, v in before.items() if "/audit-archive-results/" not in p},
            {
                p: v
                for p, v in self.snapshot().items()
                if "/audit-archive-results/" not in p
            },
        )
        package = self.paths["archive"] / (self.first["audit_id"] + ".tar.gz")
        package.write_bytes(b"bad")
        self.assertEqual(self.run_audit(action="verify")["status"], "failed")
        self.assertTrue(retired.exists())

    def test_unsafe_archives_expansion_and_truncation_are_rejected(self):
        files = audit.read_directory(self.paths["online"] / self.first["audit_id"])
        raw = audit.archive_bytes(files, "assets", self.first["audit_id"], self.f.sha)
        target = self.f.root / "test.tar.gz"
        for payload in (
            raw[:-10],
            gzip.compress(b"x" * (audit.TAR_LIMIT + 1)),
            b"not gzip",
        ):
            target.write_bytes(payload)
            with self.assertRaises(Exception):
                audit.verify_archive(target, "assets", self.first["audit_id"])
        for name, kind in (
            ("../intent.json", tarfile.REGTYPE),
            ("intent.json", tarfile.SYMTYPE),
        ):
            stream = io.BytesIO()
            with tarfile.open(fileobj=stream, mode="w") as tar:
                member = tarfile.TarInfo(name)
                member.type = kind
                member.linkname = "/private"
                tar.addfile(member, io.BytesIO())
            target.write_bytes(gzip.compress(stream.getvalue()))
            with self.assertRaises(ValueError):
                audit.verify_archive(target, "assets", self.first["audit_id"])

    def test_limits_links_and_busy_do_not_mutate_evidence(self):
        before = self.snapshot()
        with patch.object(audit, "MAX_ENTRIES", 0):
            with self.assertRaises(ValueError):
                self.archive(apply=True)
        with patch.object(audit, "SCAN_SECONDS", -1):
            with self.assertRaises(TimeoutError):
                self.run_audit()
        import fcntl

        with (self.f.state / "worker.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertEqual(self.archive(apply=True)["status"], "busy")
        self.assertEqual(before, self.snapshot())
        directory = self.paths["online"] / self.first["audit_id"]
        source = directory / "source.json"
        source.unlink()
        source.symlink_to(self.helper.source)
        self.assertEqual(self.archive(apply=True)["archived_records"], 0)

    def test_scale_above_info_cap_archives_back_below_it(self):
        for i in range(520):
            self.terminal(started=time.time() - 10000 + i)
        self.assertEqual(self.helper.row()["archive_hint"], "scan-limit")
        result = self.archive(apply=True, max_records=100)
        self.assertEqual(result["archived_records"], 100)
        self.assertLess(result["projected_online_records"], 512)
        row = self.helper.row()
        self.assertEqual(row["result_status"], "ok")
        self.assertEqual(row["attempt"]["status"], "closed")
        self.assertEqual(self.run_audit(action="verify", limit=1000)["total"], 100)

    def test_sha_scope_cannot_bypass_repository_latest_and_batch_limits(self):
        other = self.terminal(sha=self.f.current, started=time.time() - 1000)
        result = self.archive(sha=self.f.current, apply=True, max_records=1)
        self.assertEqual(result["archived_records"], 1)
        self.assertFalse((self.paths["online"] / other).exists())
        self.assertTrue((self.paths["online"] / self.second["audit_id"]).exists())

    def test_cli_preview_apply_query_and_option_restrictions(self):
        for command, apply in (
            ("restore-audit-list", False),
            ("restore-audit-archive", False),
            ("restore-audit-archive", True),
            ("restore-audit-verify", False),
        ):
            args = ["open_assets", command, "assets", self.f.sha] + (
                ["--apply"] if apply else []
            )
            with (
                patch.object(core, "load", return_value=self.config),
                patch.object(
                    audit, "run", return_value=dict(status="complete")
                ) as called,
                patch("sys.argv", args),
                patch("sys.stdout", io.StringIO()),
            ):
                core.main()
                self.assertEqual(called.call_args.kwargs["apply"], apply)
        for args in (
            ["restore-audit-list", "assets", "--apply"],
            [
                "close-restore",
                "assets",
                self.f.sha,
                "--reason",
                "reviewed",
                "--keep-last",
                "1",
            ],
            ["restore-audit-archive", "assets", "--limit", "2"],
        ):
            with (
                patch("sys.argv", ["open_assets", *args]),
                patch("sys.stderr", io.StringIO()),
            ):
                with self.assertRaises(SystemExit) as stopped:
                    core.main()
                self.assertEqual(stopped.exception.code, 2)
        for kwargs in (
            dict(keep_last=0),
            dict(max_records=501),
            dict(min_age_days=-1),
            dict(limit=0),
        ):
            with self.assertRaises(ValueError):
                self.run_audit(**kwargs)
