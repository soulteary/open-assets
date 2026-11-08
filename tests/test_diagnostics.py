"""Doctor distinguishes scheduling, execution, and actual sample ages."""

import json
import hashlib
import io
import time
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from open_assets import diagnostics as d


def unit(name="info", active="active", **properties):
    service = dict(
        Id=f"open-assets-{name}.service",
        LoadState="loaded",
        ActiveState="inactive",
        Result="success",
        ExecMainCode="1",
        ExecMainStatus="0",
        ExecMainStartTimestampMonotonic="990000000",
        ExecMainExitTimestampMonotonic="995000000",
    )
    service.update(properties)
    return (
        f"Id=open-assets-{name}.timer\nLoadState=loaded\nActiveState={active}\nUnitFileState=enabled\n\n"
        + "\n".join(f"{k}={v}" for k, v in service.items())
    )


class Diagnostics(unittest.TestCase):
    def test_required_and_optional_absent(self):
        for name, required in (
            ("info", True),
            ("logrotate", True),
            ("repair", False),
            ("retention", False),
            ("object-retention", False),
        ):
            row = d.timer(None, name, Mock(return_value=unit(name, "inactive")), 1000)
            self.assertEqual(row["status"], "attention" if required else "optional")
            self.assertEqual(row["required"], required)
            self.assertNotIn("systemctl start", row["hint"])

    def test_execution_not_timer_active(self):
        cases = (
            ({}, "ok"),
            ({"Result": "exit-code", "ExecMainStatus": "1"}, "failed"),
            ({"Result": "timeout"}, "failed"),
            ({"ExecMainCode": "2", "ExecMainStatus": "9"}, "failed"),
            (
                {
                    "ExecMainStartTimestampMonotonic": "0",
                    "ExecMainExitTimestampMonotonic": "0",
                },
                "never-or-unknown",
            ),
            ({"ExecMainExitTimestampMonotonic": "1010000000"}, "never-or-unknown"),
            (
                {"ConditionResult": "no", "ConditionTimestampMonotonic": "999000000"},
                "condition-skipped",
            ),
            ({"ActiveState": "activating"}, "running"),
        )
        for values, reason in cases:
            with self.subTest(reason=reason):
                row = d.timer(None, "info", Mock(return_value=unit(**values)), 1000)
                self.assertEqual(row["reason"], reason)
                self.assertEqual(
                    row["status"], "ok" if reason in ("ok", "running") else "attention"
                )
        row = d.timer(None, "info", Mock(return_value=unit()), 1200)
        self.assertEqual(row["reason"], "overdue")
        self.assertEqual(row["age_seconds"], 205)

    def test_optional_failure_is_visible_without_recommending_cleanup(self):
        for active in ("active", "inactive"):
            row = d.timer(
                None,
                "retention",
                Mock(return_value=unit("retention", active, Result="exit-code")),
                1000,
            )
            self.assertEqual(row["status"], "attention")
            self.assertNotIn("systemctl start", row["hint"])
            self.assertIn("不要直接补跑", row["hint"])

    def test_unavailable_or_corrupt_systemd_does_not_leak_output(self):
        for inspect in (
            Mock(return_value="secret"),
            Mock(return_value=unit(ExecMainStatus="secret")),
            Mock(side_effect=subprocess.TimeoutExpired("secret", 20)),
            Mock(side_effect=OSError("secret")),
        ):
            row = d.timer(None, "info", inspect, 1000)
            self.assertEqual(row["status"], "attention")
            self.assertNotIn("secret", json.dumps(row))

    def test_cache_sample_age_status_and_clock(self):
        config = dict(repositories=[dict(key="assets")])
        summary = dict(
            status="ok",
            interval_seconds=900,
            repositories=[dict(key="assets", status="ok", measured_at=9990)],
        )
        with (
            tempfile.TemporaryDirectory() as temp,
            patch.object(d, "local_summary", return_value=summary),
            patch.object(d.object_info, "summary", return_value=summary),
            patch.object(d.audit_info, "summary", return_value=summary),
            patch.object(
                d.resource_info,
                "summary",
                return_value=dict(status="ok", interval_seconds=900, measured_at=9990),
            ),
        ):
            root = Path(temp)
            before = list(root.iterdir())
            rows = d.caches(root, root, config, now=10000)
            self.assertTrue(all(r["status"] == "ok" for r in rows))
            for measured, status, reason in (
                (8000, "ok", "stale"),
                (10010, "ok", "clock-skew"),
                (None, "ok", "never-or-invalid"),
                (9990, "unavailable", "collector-unavailable"),
                (9990, "scanning", "collector-scanning"),
            ):
                summary["repositories"][0].update(measured_at=measured, status=status)
                rows = d.caches(root, root, config, now=10000)
                self.assertEqual(rows[1]["reason"], reason)
                self.assertEqual(rows[1]["status"], "attention")
                self.assertIn("systemctl start", rows[1]["hint"])
            self.assertEqual(list(root.iterdir()), before)

    def test_missing_cache_and_repository_are_not_healthy(self):
        with tempfile.TemporaryDirectory() as temp:
            rows = d.caches(
                Path(temp),
                Path(temp),
                dict(repositories=[dict(key="assets")]),
                now=10000,
            )
        self.assertEqual(len(rows), 4)
        self.assertTrue(all(r["status"] == "attention" for r in rows))

    @unittest.skipUnless(Path("/proc/uptime").exists(), "Linux systemd monotonic clock")
    def test_real_cache_files_cli_exit_codes_and_no_scanning(self):
        import test_operator as fixture
        from open_assets import operator, resources

        setup = fixture.Operator()
        setup.setUp()
        self.addCleanup(setup.doCleanups)
        root, config = setup.root, setup.config
        data = root / "data"
        folder = data / "info"
        folder.mkdir(parents=True)
        state = data / "publisher"
        state.mkdir()
        for lock in ("worker", "queue"):
            (state / (lock + ".lock")).touch()
        (root / ".env").write_text("DATA_ROOT=" + str(data) + "\n")
        now = time.time()

        def write(name, value):
            (folder / name).write_text(json.dumps(value))

        write("snapshot.json", dict(schema_version=1, collected_at=now))
        write(
            "repositories-cache.json",
            dict(
                schema_version=1,
                config_digest=hashlib.sha256(
                    (root / "private/repositories.yaml").read_bytes()
                ).hexdigest(),
                repositories=dict(
                    assets=dict.fromkeys(d.storage_info.METRICS, 0)
                    | dict(measured_at=now)
                ),
                failures=[],
            ),
        )
        _, keys, digest = d.object_info.scope(root / "private/repositories.yaml")
        objects = dict(
            schema_version=1,
            scope=digest,
            repositories=dict(
                assets=dict(
                    status="ok",
                    complete=dict(
                        objects=0, bytes=0, started_at=now - 1, measured_at=now
                    ),
                )
            ),
        )
        write("objects-cache.json", objects)
        sample = dict(
            measured_at=now,
            protection_counts={},
            invalid_records=0,
            candidate_records=0,
            candidate_bytes=0,
            eligible_records=0,
        )
        sample.update(
            {k: dict(records=0, bytes=0, disk_bytes=0) for k in d.audit_info.LOCATIONS}
        )
        write(
            "audit-cache.json",
            dict(
                schema_version=1,
                scope=d.audit_info.identity(keys),
                repositories=dict(assets=dict(status="ok", sample=sample)),
            ),
        )
        cache = d.resource_info.collect(
            dict(config, state_root=str(state)), {}, now=now
        )
        write("resource-cache.json", cache)
        containers = [
            dict(Service=name, State="running", Health="healthy")
            for name in operator.SERVICES
        ]

        def inspect(args, **kwargs):
            if args[0] == "docker":
                return json.dumps(containers)
            name = args[2].removeprefix("open-assets-").removesuffix(".timer")
            uptime = (
                float(Path("/proc/uptime").read_text().split()[0])
                if Path("/proc/uptime").exists()
                else 1000
            )
            return unit(
                name,
                "active" if d.TIMERS[name][0] else "inactive",
                ExecMainStartTimestampMonotonic=str(int((uptime - 2) * 1e6)),
                ExecMainExitTimestampMonotonic=str(int((uptime - 1) * 1e6)),
            )

        before = {p: p.read_bytes() for p in root.rglob("*") if p.is_file()}
        with (
            patch.object(operator, "inspect_command", side_effect=inspect),
            patch.object(resources, "usage", side_effect=AssertionError("scan")),
            patch.object(resources, "queue_usage", side_effect=AssertionError("scan")),
            patch.object(d.storage_info, "measure", side_effect=AssertionError("scan")),
            patch.object(d.object_info, "client", side_effect=AssertionError("S3")),
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            self.assertEqual(
                operator.main(["--root", str(root), "doctor", "--json"]), 0
            )
            report = json.loads(output.getvalue())
            self.assertTrue(all(r["status"] == "ok" for r in report["caches"]))
            self.assertEqual(
                sum(r["status"] == "optional" for r in report["timers"]), 3
            )
            self.assertEqual(
                before, {p: p.read_bytes() for p in root.rglob("*") if p.is_file()}
            )
            objects["repositories"]["assets"]["complete"]["measured_at"] = now - 2000
            objects["repositories"]["assets"]["complete"]["started_at"] = now - 2001
            write("objects-cache.json", objects)
            output.seek(0)
            output.truncate()
            self.assertEqual(
                operator.main(["--root", str(root), "doctor", "--json"]), 1
            )
            report = json.loads(output.getvalue())
            self.assertEqual(
                next(r for r in report["caches"] if r["cache"] == "objects")["reason"],
                "stale",
            )
            self.assertEqual(
                next(r for r in report["checks"] if r["check"] == "info-snapshot")[
                    "status"
                ],
                "ok",
            )
            (folder / "objects-cache.json").unlink()
            self.assertTrue(
                any(r["status"] == "attention" for r in d.caches(root, data, config))
            )
