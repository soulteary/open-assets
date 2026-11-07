"""Cached resource observations must not become a frequent state traversal."""

import fcntl
import json
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

import yaml
from open_assets import resources, resource_events, resource_info as info
from open_assets import __main__ as core
from open_assets.storage_info import write_cache


class ResourceInfo(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "private").mkdir()
        self.data = self.root / "data"
        self.state = self.data / "publisher"
        self.state.mkdir(parents=True)
        for name in ("worker", "queue"):
            (self.state / (name + ".lock")).touch()
        self.c = dict(
            schema_version=1,
            state_root="/state",
            images_root="/images",
            bucket="attachments",
            endpoint="http://fixture:9000",
            revision="test",
            repositories=[dict(key="assets", enabled=True)],
            resource_limits=dict(
                queue_active_records=2,
                queue_total_records=4,
                queue_reserved_bytes=262144,
                git_repository_bytes=100,
                git_total_bytes=200,
            ),
        )
        (self.root / "private/repositories.yaml").write_text(yaml.safe_dump(self.c))
        (self.root / ".env").write_text("DATA_ROOT=" + str(self.data) + "\n")
        self.host = dict(self.c, state_root=str(self.state))
        folder = self.state / "git/assets/objects"
        folder.mkdir(parents=True)
        (folder / "fixture").write_bytes(b"x" * 80)
        self.now = time.time()
        self.path = self.data / "info/resource-cache.json"

    def sample(self, previous=None, **kwargs):
        return info.collect(self.host, previous or {}, now=self.now, **kwargs)

    def test_budget_counts_and_alerts_share_admission_accounting(self):
        core.enqueue(self.host, "assets", "a" * 40, "fixture")
        core.enqueue(self.host, "assets", "b" * 40, "fixture")
        cache = self.sample()
        self.assertEqual(cache["status"], "ok")
        self.assertEqual(cache["sample"]["queue"], resources.queue_usage(self.host))
        self.assertEqual(cache["sample"]["queue"]["reserved_bytes"], 131072)
        write_cache(self.path, cache)
        result = info.summary(self.root, self.data, self.now)
        self.assertEqual(result["repositories"], [dict(key="assets", bytes=80)])
        self.assertEqual(
            {a["code"] for a in result["alerts"]},
            {"git_repository_bytes", "queue_active_records"},
        )
        self.assertFalse(result["stale"])
        with self.assertRaises(resources.ResourceLimit):
            core.enqueue(self.host, "assets", "c" * 40, "refused")
        result = info.summary(self.root, self.data, self.now)
        self.assertEqual(result["events"]["queue"]["code"], "queue_active_records")
        self.assertEqual(result["events"]["queue"]["key"], "assets")

    def test_cache_hit_and_summary_never_scan(self):
        cache = self.sample()
        write_cache(self.path, cache)
        with (
            patch.object(resources, "usage", side_effect=AssertionError("rescan")),
            patch.object(
                resources, "queue_usage", side_effect=AssertionError("rescan")
            ),
        ):
            self.assertEqual(self.sample(cache), cache)
            self.assertEqual(
                info.summary(self.root, self.data, self.now)["status"], "ok"
            )
            self.assertEqual(
                info.summary(self.root, self.data, self.now)["git_bytes"], 80
            )

    def test_busy_failure_and_stale_preserve_values_without_zero_success(self):
        cache = self.sample()
        with (self.state / "worker.lock").open() as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            busy = self.sample(cache, force=True)
        self.assertEqual(busy["status"], "busy")
        self.assertEqual(busy["sample"], cache["sample"])
        self.assertEqual(self.sample(busy, force=True)["status"], "ok")
        with patch.object(resources, "usage", side_effect=OSError("private data")):
            failed = self.sample(cache, force=True)
        write_cache(self.path, failed)
        result = info.summary(self.root, self.data, self.now + 1801)
        self.assertEqual(result["status"], "unavailable")
        self.assertEqual(result["git_bytes"], 80)
        self.assertTrue(result["stale"])
        self.assertNotIn("private data", json.dumps(result))

    def test_configuration_change_corruption_and_missing_are_unknown(self):
        self.assertEqual(info.summary(self.root, self.data)["status"], "unavailable")
        cache = self.sample()
        write_cache(self.path, cache)
        cache["sample"]["queue"]["total_records"] = True
        write_cache(self.path, cache)
        self.assertEqual(info.summary(self.root, self.data)["status"], "unavailable")
        cache = self.sample()
        write_cache(self.path, cache)
        self.c["resource_limits"]["git_total_bytes"] = 201
        (self.root / "private/repositories.yaml").write_text(yaml.safe_dump(self.c))
        self.assertEqual(info.summary(self.root, self.data)["status"], "unavailable")

    def test_special_files_limits_deadline_and_no_mutation(self):
        before = (self.state / "git/assets/objects/fixture").read_bytes()
        (self.state / "git/assets/objects/link").symlink_to(self.root)
        self.assertEqual(self.sample()["status"], "unavailable")
        (self.state / "git/assets/objects/link").unlink()
        with patch.object(info, "MAX_SECONDS", -1):
            self.assertEqual(self.sample()["status"], "unavailable")
        self.host["resource_limits"] = dict(scan_entries=1)
        self.assertEqual(self.sample()["status"], "unavailable")
        self.assertEqual(
            (self.state / "git/assets/objects/fixture").read_bytes(), before
        )
        self.assertFalse(list(self.state.glob("jobs/*")))

    def test_empty_state_and_event_without_capacity_cache(self):
        (self.state / "git/assets/objects/fixture").unlink()
        cache = self.sample()
        self.assertEqual(cache["status"], "ok")
        self.assertEqual(cache["sample"]["git_bytes"], 0)
        self.assertEqual(cache["sample"]["queue"]["total_records"], 0)
        with core.lock(self.host, "queue"):
            resource_events.record(
                self.host, resources.ResourceLimit("git_total_bytes"), "assets"
            )
        result = info.summary(self.root, self.data)
        self.assertEqual(result["status"], "unavailable")
        self.assertEqual(result["events"]["git"]["code"], "git_total_bytes")

    def test_invalid_event_and_dangling_queue_are_not_green(self):
        write_cache(self.path, self.sample())
        (self.state / "resource-limit-events.json").write_text(
            '{"git":{"code":"unknown","key":"assets","at":1}}'
        )
        self.assertEqual(
            info.summary(self.root, self.data)["event_status"], "unavailable"
        )
        (self.state / "job-archive").symlink_to(self.root / "missing")
        self.assertEqual(self.sample()["status"], "unavailable")

    def test_events_are_bounded_filtered_and_do_not_mask_refusal(self):
        for _ in range(20):
            with core.lock(self.host, "queue"):
                resource_events.record(
                    self.host,
                    resources.ResourceLimit("scan_entries"),
                    "assets",
                    category="queue",
                )
                resource_events.record(
                    self.host, resources.ResourceLimit("git_total_bytes"), "assets"
                )
                resource_events.record(self.host, ValueError("secret"), "assets")
        path = self.state / "resource-limit-events.json"
        value = json.loads(path.read_text())
        self.assertEqual(set(value), {"queue", "git"})
        self.assertLess(path.stat().st_size, 1024)
        self.assertEqual(
            resource_events.clean(
                dict(
                    git=dict(code="unknown", at=self.now, key="assets", secret="hidden")
                ),
                ["assets"],
            ),
            {},
        )
        with patch.object(core, "atomic", side_effect=OSError("disk full")):
            resource_events.record(
                self.host, resources.ResourceLimit("git_total_bytes"), "assets"
            )


if __name__ == "__main__":
    unittest.main()
