import unittest
from pathlib import Path
import tempfile

from open_assets import health, repair


class HealthRepair(unittest.TestCase):
    def test_heartbeat_reader_rejects_symlinks_and_oversized_input(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "beat"
            path.write_text("x" * 4097)
            with self.assertRaises(ValueError):
                health.read_worker_heartbeat(path)
            link = Path(tmp) / "link"
            link.symlink_to(path)
            with self.assertRaises(OSError):
                health.read_worker_heartbeat(link)

    def row(self, service="info", **changes):
        row = dict(
            id="container",
            service=service,
            scope="asset-stack/" + service,
            running=True,
            paused=False,
            health="unhealthy",
        )
        row.update(changes)
        return row

    def observed(self, service="info"):
        return {
            "unhealthy": {"asset-stack/" + service: {"id": "container", "since": 1000}},
            "events": [],
        }

    def test_requires_persistent_failure_and_skips_stopped_or_paused(self):
        row = self.row()
        state, actions = repair.plan([row], {}, 1000)
        self.assertEqual(actions, [])
        self.assertEqual(repair.plan([row], state, 1029)[1], [])
        self.assertEqual(len(repair.plan([row], state, 1030)[1]), 1)
        for changes in (
            {"running": False},
            {"paused": True},
            {"health": "healthy"},
            {"health": None},
        ):
            self.assertEqual(repair.plan([self.row(**changes)], state, 1100)[1], [])

    def test_restart_cooldown_and_hourly_budget(self):
        state = self.observed()
        state["events"] = [
            dict(
                scope="asset-stack/info",
                time=1900,
                action="restart",
                result="restarted",
            )
        ]
        self.assertEqual(repair.plan([self.row()], state, 2000)[1], [])
        self.assertEqual(len(repair.plan([self.row()], state, 2600)[1]), 1)
        state["events"] = [
            dict(scope="asset-stack/info", time=t, action="restart", result="failed")
            for t in (2100, 2500, 2800)
        ]
        self.assertEqual(repair.plan([self.row()], state, 3500)[1], [])

    def test_global_budget_and_project_scope(self):
        state = self.observed()
        state["events"] = [
            dict(
                scope="asset-stack/" + s,
                time=1900,
                action="restart",
                result="restarted",
            )
            for s in ("nginx", "oc", "webhook")
        ]
        self.assertEqual(repair.plan([self.row()], state, 2000)[1], [])
        inspections = [
            dict(
                Id="other",
                Config={
                    "Labels": {
                        "com.docker.compose.project": "unrelated",
                        "com.docker.compose.service": "info",
                    }
                },
                State={"Running": True, "Health": {"Status": "unhealthy"}},
            )
        ]
        self.assertEqual(repair.eligible(inspections, "traefik"), [])

    def test_worker_liveness_and_job_deadline(self):
        record = dict(updated_at=1000, progress_at=1000, job_started_at=None)
        self.assertTrue(health.worker_healthy(record, 1010))
        self.assertFalse(health.worker_healthy(record, 1070))
        record.update(updated_at=2000, job_started_at=1000)
        self.assertTrue(health.worker_healthy(record, 2000))
        record["updated_at"] = 3000
        self.assertFalse(health.worker_healthy(record, 3000))
