"""Audit measurement is cached; fast host reads never traverse packages."""

import copy
import json
import time
import unittest
from unittest.mock import patch

from open_assets import audit_info as info, audit_observation as observation
from open_assets import restore_audit as audit
import test_restore_audit as fixture


class AuditInfo(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture.RestoreAudit()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.config = self.fixture.config
        self.root = self.fixture.helper.helper.host()
        self.data = self.root / "data"
        (self.data / "info").mkdir()
        self.path = self.data / "info/audit-cache.json"

    def collect(self, previous=None, force=False):
        cache = info.collect(self.config, previous or {}, force=force)
        self.path.write_text(json.dumps(cache))
        return cache

    def row(self):
        return info.summary(self.root, self.data)["repositories"][0]

    def test_counts_logical_and_allocated_bytes_match_locations(self):
        self.fixture.archive(apply=True)
        self.collect()
        row = self.row()
        self.assertEqual(row["online"]["records"], 1)
        self.assertEqual(row["archive"]["records"], 1)
        self.assertEqual(row["retired"]["records"], 0)
        for location, path in self.fixture.paths.items():
            if path.exists():
                expected = sum(p.stat().st_size for p in path.rglob("*") if p.is_file())
                disk = sum(p.stat().st_blocks * 512 for p in [path, *path.rglob("*")])
                self.assertEqual(row[location]["bytes"], expected)
                self.assertEqual(row[location]["disk_bytes"], disk)
        self.assertEqual(row["archival"]["attempt"]["archived_records"], 1)
        self.assertEqual(row["archival"]["attempt"]["status"], "complete")

    def test_interval_hit_and_fast_summary_do_not_scan_archives(self):
        cache = self.collect()
        with (
            patch.object(audit, "scan", side_effect=AssertionError("must not scan")),
            patch.object(
                info, "measure", side_effect=AssertionError("must not measure")
            ),
        ):
            self.assertEqual(info.collect(self.config, cache), cache)
            self.assertEqual(self.row()["online"]["records"], 2)
        self.assertEqual(json.loads(self.path.read_text()), cache)

    def test_force_and_expired_interval_refresh(self):
        cache = self.collect()
        real = audit.scan
        with patch.object(audit, "scan", wraps=real) as scan:
            self.collect(cache, force=True)
            self.assertEqual(scan.call_count, 1)
        cache["repositories"]["assets"]["attempted_at"] -= info.INTERVAL + 1
        with patch.object(audit, "scan", wraps=real) as scan:
            self.collect(cache)
            self.assertEqual(scan.call_count, 1)

    def test_failed_measurement_keeps_old_values_and_marks_them_unavailable(self):
        cache = self.collect()
        old = copy.deepcopy(cache["repositories"]["assets"]["sample"])
        with patch.object(audit, "scan", side_effect=OSError("secret endpoint")):
            cache = self.collect(cache, force=True)
        self.assertEqual(cache["repositories"]["assets"]["sample"], old)
        self.assertEqual(self.row()["status"], "unavailable")
        self.assertNotIn("secret endpoint", json.dumps(cache))
        stale = info.summary(
            self.root, self.data, now=time.time() + info.INTERVAL * 2 + 1
        )["repositories"][0]
        self.assertTrue(stale["stale"])

    def test_empty_repository_is_zero_but_missing_cache_is_unknown(self):
        self.assertEqual(info.summary(self.root, self.data)["status"], "unavailable")
        config = copy.deepcopy(self.config)
        config["repositories"] = [dict(config["repositories"][0], key="empty")]
        cache = info.collect(config, {})
        sample = cache["repositories"]["empty"]["sample"]
        self.assertTrue(
            all(
                sample[name]["records"] == 0 and sample[name]["bytes"] == 0
                for name in info.LOCATIONS
            )
        )

    def test_busy_worker_retains_sample_and_does_not_advance_attempt_time(self):
        import fcntl

        cache = self.collect()
        before = copy.deepcopy(cache["repositories"]["assets"])
        with (self.fixture.f.state / "worker.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            busy = self.collect(cache, force=True)["repositories"]["assets"]
        self.assertEqual(busy["status"], "busy")
        self.assertEqual(busy["sample"], before["sample"])
        self.assertEqual(busy["attempted_at"], before["attempted_at"])

    def test_partial_retirement_and_invalid_records_remain_visible(self):
        self.fixture.interrupt_retirement()
        self.collect()
        row = self.row()
        self.assertEqual(row["archive"]["records"], 1)
        self.assertEqual(row["retired"]["records"], 1)
        self.assertEqual(row["archival"]["attempt"]["status"], "failed")
        self.assertTrue(row["archival"]["attempt"]["partial_possible"])
        package = self.fixture.paths["archive"] / (
            self.fixture.first["audit_id"] + ".tar.gz"
        )
        package.write_bytes(b"bad")
        self.collect(force=True)
        row = self.row()
        self.assertGreater(row["invalid_records"], 0)
        self.assertGreater(row["protection_counts"]["invalid"], 0)

    def test_recent_policy_protections_are_separate_from_last_custom_apply(self):
        self.fixture.archive(apply=True)
        self.collect()
        row = self.row()
        self.assertEqual(row["archival"]["attempt"]["keep_last"], 1)
        self.assertEqual(info.summary(self.root, self.data)["policy"]["keep_last"], 20)
        self.assertGreater(row["protection_counts"]["keep-last"], 0)

    def test_apply_observation_preserves_history_failure_and_unresolved_intent(self):
        self.fixture.archive(apply=True)
        old = observation.read(self.config, "assets")["last_complete"]
        # Nothing else is eligible; interrupt the final result write after persisted intent.
        real = observation.persist

        def interrupted(*args, **kwargs):
            if kwargs.get("finished"):
                raise OSError("fixture result interruption")
            return real(*args, **kwargs)

        with patch.object(observation, "persist", side_effect=interrupted):
            with self.assertRaises(OSError):
                self.fixture.archive(apply=True)
        result = observation.read(self.config, "assets")
        self.assertEqual(result["attempt"]["status"], "running")
        self.assertEqual(result["last_complete"], old)

    def test_default_preview_stays_read_only_and_observation_is_filtered(self):
        before = self.fixture.snapshot()
        self.fixture.archive()
        self.assertEqual(before, self.fixture.snapshot())
        self.fixture.archive(apply=True)
        path = self.fixture.f.state / "audit-archive-results/assets.json"
        value = json.loads(path.read_text())
        value["attempt"].update(secret="private URL", sha="a" * 40)
        path.write_text(json.dumps(value))
        self.collect()
        text = json.dumps(self.row())
        self.assertNotIn("private URL", text)
        self.assertNotIn("a" * 40, text)
        self.assertNotIn("audit_id", text)
        path.write_text("{")
        self.assertEqual(self.row()["archival"]["status"], "unavailable")

    def test_malformed_cache_scope_changes_and_nonregular_measurement_are_not_zero(
        self,
    ):
        cache = self.collect()
        cache["repositories"]["assets"]["sample"]["online"]["bytes"] = True
        self.assertEqual(
            info.sanitize(cache, ["assets"])["repositories"]["assets"]["status"],
            "unavailable",
        )
        self.assertEqual(info.sanitize(cache, ["different"])["repositories"], {})
        target = (
            self.fixture.paths["online"]
            / self.fixture.first["audit_id"]
            / "source.json"
        )
        target.unlink()
        target.symlink_to(self.fixture.helper.source)
        old = self.collect(force=True)["repositories"]["assets"]
        self.assertEqual(old["status"], "unavailable")
