import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from open_assets import host_history as history, telemetry


class HostHistory(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / "host-history.json"
        self.host = dict(
            cpu_percent=12,
            cpu_count=2,
            memory=dict(used=25, total=100),
            load=[0.1, 0.2, 0.3],
            private="must never persist",
        )

    def append(self, at):
        result = history.update(self.path, self.host, at)
        self.path.write_text(json.dumps(result))
        return result

    def test_bucket_replacement_retention_and_restart(self):
        self.append(100000)
        self.append(100001)
        data = self.append(100032)
        self.assertEqual(len(data["points"]), 2)
        self.assertEqual(data["points"][0]["at"], 100001)
        data = self.append(200000)
        self.assertEqual(len(data["points"]), 1)
        self.assertNotIn("must never persist", json.dumps(data))

    def test_invalid_fields_are_missing_not_zero_and_future_is_dropped(self):
        self.append(200000)
        self.host.update(
            cpu_percent=True,
            cpu_count=False,
            load=[float("nan"), -1, True],
            memory=dict(used=101, total=100),
        )
        data = self.append(100000)
        self.assertEqual(len(data["points"]), 1)
        self.assertIsNone(data["points"][0]["cpu_percent"])
        self.assertIsNone(data["points"][0]["memory_used"])
        self.assertEqual(data["points"][0]["load"], [None] * 3)

    def test_bad_oversized_or_linked_file_never_reads_outside(self):
        for raw in ("bad", "x" * (history.MAX_BYTES + 1)):
            self.path.write_text(raw)
            self.assertEqual(
                len(history.update(self.path, self.host, 100000)["points"]), 1
            )
        self.path.unlink()
        outside = self.path.with_name("outside")
        outside.write_text("private")
        self.path.symlink_to(outside)
        self.assertEqual(len(history.update(self.path, self.host, 100000)["points"]), 1)
        self.assertEqual(outside.read_text(), "private")

    def test_snapshot_write_persists_and_embeds_identical_numeric_history(self):
        path = self.path.with_name("snapshot.json")
        with patch.object(os, "chown"):
            for at in (100000, 100032):
                telemetry.write_snapshot(
                    path, dict(schema_version=1, collected_at=at, host=self.host)
                )
        snapshot = json.loads(path.read_text())
        persisted = json.loads(self.path.read_text())
        self.assertEqual(snapshot["host_history"], persisted)
        self.assertEqual(len(persisted["points"]), 2)
        self.assertNotIn("private", json.dumps(persisted))

    def test_history_is_bounded_after_many_manual_refreshes(self):
        self.path.write_text(
            json.dumps(
                dict(
                    schema_version=1,
                    points=[dict(at=i * 30, cpu_percent=1) for i in range(4000)],
                )
            )
        )
        data = history.update(self.path, self.host, 120000)
        self.assertLessEqual(len(data["points"]), history.MAX_POINTS)
        self.assertTrue(
            all(p["at"] >= 120000 - history.RETENTION for p in data["points"])
        )
