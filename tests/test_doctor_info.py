"""Cache diagnostics without introducing checks into page or host refreshes."""

import copy
import http.client
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from open_assets import diagnostics, doctor_info as cache, info, operator


def fixture():
    return dict(
        status="ok",
        checks=[
            dict(check=k, status="ok", hint="检查通过")
            for k in (
                "configuration",
                "containers",
                "timers",
                "info-snapshot",
                "caches",
            )
        ],
        timers=[
            dict(
                timer=k,
                required=required,
                status="ok" if required else "optional",
                reason="ok" if required else "not-enabled",
                hint="无需开启可选任务",
                age_seconds=10,
                max_age_seconds=limit,
                active="active" if required else "inactive",
                result="success",
                exit_code=0,
            )
            for k, (required, limit) in diagnostics.TIMERS.items()
        ],
        caches=[
            dict(
                cache="objects",
                key="assets",
                status="ok",
                reason="fresh",
                age_seconds=5,
                max_age_seconds=1800,
                hint="样本有效",
            )
        ],
        components=[
            dict(service=k, state="running", health="healthy")
            for k in operator.SERVICES
        ],
    )


class DoctorCache(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "private").mkdir()
        (self.root / "private/repositories.yaml").write_text("fixture")
        self.data = self.root / "data"
        (self.root / ".env").write_text("DATA_ROOT=" + str(self.data) + "\n")
        self.path = self.data / "info/doctor-cache.json"
        self.now = time.time()

    def collect(self, sample=None, now=None, **kwargs):
        with patch.object(
            operator, "doctor", return_value=sample or fixture()
        ) as doctor:
            result = cache.collect(
                self.root, self.path, now=self.now if now is None else now, **kwargs
            )
        return result, doctor

    def test_interval_and_summary_are_read_only_with_no_subprocess_or_scan(self):
        self.collect()
        before = self.path.read_bytes()
        result, doctor = self.collect(now=self.now + 60)
        doctor.assert_not_called()
        self.assertEqual(result["status"], "ok")
        with (
            patch.object(operator, "doctor", side_effect=AssertionError("check")),
            patch.object(
                operator, "inspect_command", side_effect=AssertionError("command")
            ),
            patch("os.scandir", side_effect=AssertionError("scan")),
        ):
            value = cache.summary(self.root, self.data, now=self.now + 100)
        self.assertFalse(value["stale"])
        self.assertEqual(value["age_seconds"], 100)
        self.assertEqual(before, self.path.read_bytes())
        self.assertTrue(cache.summary(self.root, self.data, self.now + 601)["stale"])
        self.assertTrue(cache.summary(self.root, self.data, self.now - 10)["stale"])

    def test_attention_is_a_successful_collection_not_a_collector_failure(self):
        sample = fixture()
        sample["status"] = "attention"
        sample["timers"][0].update(
            status="attention", reason="failed", result="exit-code", exit_code=1
        )
        self.collect(sample)
        value = cache.summary(self.root, self.data, self.now)
        self.assertEqual(value["status"], "ok")
        self.assertEqual(value["report"]["status"], "attention")
        self.assertEqual(value["report"]["timers"][0]["exit_code"], 1)

    def test_failure_and_interruption_preserve_prior_report_without_error_text(self):
        initial, _ = self.collect()
        with patch.object(
            operator, "doctor", side_effect=RuntimeError("private secret")
        ):
            failed = cache.collect(self.root, self.path, force=True, now=self.now + 1)
        self.assertEqual(failed["status"], "unavailable")
        self.assertEqual(failed["report"], initial["report"])
        self.assertEqual(failed["measured_at"], self.now)
        self.assertNotIn("private secret", self.path.read_text())
        with patch.object(operator, "doctor", side_effect=KeyboardInterrupt):
            with self.assertRaises(KeyboardInterrupt):
                cache.collect(self.root, self.path, force=True, now=self.now + 2)
        self.assertEqual(
            cache.summary(self.root, self.data, self.now + 2)["status"], "running"
        )
        result, doctor = self.collect(now=self.now + 3)
        doctor.assert_called_once()
        self.assertEqual(result["status"], "ok")

    def test_invalid_missing_oversized_or_scope_changed_cache_never_succeeds(self):
        self.assertTrue(cache.summary(self.root, self.data)["stale"])
        self.collect()
        (self.root / "private/repositories.yaml").write_text("changed")
        self.assertNotIn("report", cache.summary(self.root, self.data))
        self.path.write_text("{")
        self.assertEqual(cache.summary(self.root, self.data)["status"], "unavailable")
        self.path.write_text("x" * (cache.LIMIT + 1))
        self.assertNotIn("report", cache.summary(self.root, self.data))
        self.path.unlink()
        self.path.symlink_to(self.root / ".env")
        self.assertNotIn("report", cache.summary(self.root, self.data))

    def test_filter_discards_raw_fields_and_rejects_incomplete_samples(self):
        sample = fixture()
        sample["raw"] = "private secret"
        sample["timers"][0]["Environment"] = "private secret"
        sample["components"][0]["state"] = "private secret"
        self.assertNotIn("private secret", json.dumps(cache.report(sample)))
        for key in ("timers", "checks"):
            bad = copy.deepcopy(sample)
            bad[key] = []
            with self.assertRaises(ValueError):
                cache.report(bad)
        bad = copy.deepcopy(sample)
        bad["caches"][0]["key"] = "../private"
        with self.assertRaises(ValueError):
            cache.report(bad)

    def test_interval_validation_and_force_do_not_skip_lock(self):
        self.collect()
        _, doctor = self.collect(now=self.now + 10, force=True)
        doctor.assert_called_once()
        (self.root / ".env").write_text("INFO_DOCTOR_INTERVAL_SECONDS=0\n")
        self.assertEqual(cache.summary(self.root, self.data)["status"], "unavailable")
        (self.root / ".env").write_text("DATA_ROOT=" + str(self.data) + "\n")
        import fcntl

        with (
            patch.object(cache.os, "geteuid", return_value=0),
            (self.path.parent / ".doctor-collector.lock").open("a") as lock,
        ):
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(operator, "doctor", side_effect=AssertionError("locked")):
                self.assertEqual(cache.main(["--root", str(self.root), "--force"]), 0)

    def test_page_refresh_only_reads_snapshot_and_retains_diagnostic_payload(self):
        self.collect()
        path = self.path.parent / "snapshot.json"
        path.write_text(
            json.dumps(
                dict(
                    schema_version=1,
                    collected_at=time.time(),
                    diagnostics=cache.summary(self.root, self.data),
                )
            )
        )
        server = info.http.server.ThreadingHTTPServer(("127.0.0.1", 0), info.Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with (
                patch.object(info, "SNAPSHOT", path),
                patch.object(
                    operator, "doctor", side_effect=AssertionError("HTTP check")
                ),
                patch.object(
                    cache, "collect", side_effect=AssertionError("HTTP collect")
                ),
            ):
                conn = http.client.HTTPConnection("127.0.0.1", server.server_port)
                for _ in range(3):
                    conn.request("GET", "/api/status")
                    response = conn.getresponse()
                    self.assertEqual(response.status, 200)
                    self.assertEqual(
                        json.loads(response.read())["diagnostics"]["report"]["status"],
                        "ok",
                    )
                conn.close()
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
