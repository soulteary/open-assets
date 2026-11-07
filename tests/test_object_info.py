"""S3 metadata collection must remain bounded, cached and read-only."""

import json
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

import yaml

from open_assets import object_info as oi


class ObjectInfoTests(unittest.TestCase):
    def setUp(self):
        self.now = time.time() - 1
        self.config = dict(
            bucket="attachments", endpoint="https://storage.internal:9000"
        )
        self.keys = ["images", "images-two"]
        self.client = Mock()

    def page(self, key="images", size=8, **extra):
        return dict(
            Contents=[dict(Key=f"repositories/{key}/releases/a/test.bin", Size=size)],
            IsTruncated=False,
            **extra,
        )

    def scan(self, previous=None, keys=None):
        return oi.scan(
            self.config,
            keys or self.keys,
            "digest",
            previous or {},
            self.client,
            clock=lambda: self.now,
            sleep=lambda _: None,
        )

    def test_empty_and_nonempty_counts_prefix_and_no_object_reads(self):
        self.client.list_objects_v2.side_effect = [self.page(), dict(IsTruncated=False)]
        result = self.scan()
        self.assertEqual(result["repositories"]["images"]["complete"]["bytes"], 8)
        self.assertEqual(result["repositories"]["images-two"]["complete"]["objects"], 0)
        self.assertEqual(len(self.client.method_calls), 2)
        for call, key in zip(self.client.method_calls, self.keys):
            self.assertEqual(call[0], "list_objects_v2")
            self.assertEqual(
                call.kwargs,
                dict(Bucket="attachments", Prefix=f"repositories/{key}/", MaxKeys=1000),
            )

    def test_repeat_within_interval_has_no_requests(self):
        self.client.list_objects_v2.side_effect = [self.page(), dict(IsTruncated=False)]
        result = self.scan()
        self.client.reset_mock()
        self.scan(result)
        self.client.assert_not_called()
        self.assertEqual(self.client.method_calls, [])

    def test_paginated_progress_is_resumed_and_never_becomes_a_partial_total(self):
        self.client.list_objects_v2.return_value = dict(
            self.page(), IsTruncated=True, NextContinuationToken="private-token"
        )
        with patch.object(oi, "MAX_PAGES", 1):
            first = self.scan(keys=["images"])
        row = first["repositories"]["images"]
        self.assertEqual(row["status"], "scanning")
        self.assertNotIn("complete", row)
        self.now += 901
        self.client.list_objects_v2.return_value = self.page(size=12)
        second = self.scan(first, keys=["images"])
        self.assertEqual(second["repositories"]["images"]["complete"]["objects"], 2)
        self.assertEqual(second["repositories"]["images"]["complete"]["bytes"], 20)
        self.assertEqual(
            self.client.list_objects_v2.call_args.kwargs["ContinuationToken"],
            "private-token",
        )
        self.assertNotIn("progress", second["repositories"]["images"])

    def test_failures_preserve_complete_values_and_reset_invalid_cursor(self):
        self.client.list_objects_v2.return_value = self.page()
        old = self.scan(keys=["images"])
        complete = old["repositories"]["images"]["complete"].copy()
        self.now += 901
        self.client.list_objects_v2.side_effect = RuntimeError(
            "private endpoint/key/secret"
        )
        result = self.scan(old, keys=["images"])
        row = result["repositories"]["images"]
        self.assertEqual(row["complete"], complete)
        self.assertEqual(row["status"], "unavailable")
        self.assertNotIn("private endpoint", json.dumps(result))
        self.assertNotIn("progress", row)

    def test_malformed_or_out_of_prefix_listing_is_not_counted(self):
        for response in [
            dict(
                Contents=[dict(Key="repositories/images-two/x", Size=1)],
                IsTruncated=False,
            ),
            dict(
                Contents=[dict(Key="repositories/images/x", Size=-1)], IsTruncated=False
            ),
            dict(Contents=[], IsTruncated=True),
            dict(Contents=[]),
        ]:
            with self.subTest(response=response):
                self.client.list_objects_v2.return_value = response
                row = self.scan(keys=["images"])["repositories"]["images"]
                self.assertEqual(row["status"], "unavailable")
                self.assertNotIn("complete", row)

    def test_page_budget_and_round_robin_prevent_single_repository_monopoly(self):
        self.client.list_objects_v2.side_effect = [
            dict(self.page(), IsTruncated=True, NextContinuationToken="cursor"),
            self.page("images-two"),
        ]
        with patch.object(oi, "MAX_PAGES", 2):
            result = self.scan()
        self.assertEqual(result["request_pages"], 2)
        self.assertEqual(result["repositories"]["images-two"]["status"], "ok")
        self.assertEqual(result["repositories"]["images"]["status"], "scanning")

    def test_expired_scan_restarts_without_old_counts_or_token(self):
        old = dict(
            scope="digest",
            repositories={
                "images": dict(
                    status="scanning",
                    attempted_at=self.now - 901,
                    progress=dict(
                        started_at=self.now - 86401,
                        objects=99,
                        bytes=999,
                        token="expired",
                    ),
                )
            },
        )
        self.client.list_objects_v2.return_value = self.page()
        result = self.scan(old, keys=["images"])
        self.assertNotIn(
            "ContinuationToken", self.client.list_objects_v2.call_args.kwargs
        )
        self.assertEqual(result["repositories"]["images"]["complete"]["objects"], 1)

    def test_untouched_partial_repository_keeps_its_attempt_time_for_fairness(self):
        self.client.list_objects_v2.return_value = dict(
            self.page(), IsTruncated=True, NextContinuationToken="cursor"
        )
        with patch.object(oi, "MAX_PAGES", 1):
            first = self.scan()
            attempted = first["repositories"]["images"]["attempted_at"]
            self.now += 901
            self.client.list_objects_v2.return_value = self.page("images-two")
            second = self.scan(first)
        self.assertEqual(second["repositories"]["images"]["attempted_at"], attempted)
        self.assertEqual(second["repositories"]["images-two"]["status"], "ok")

    def test_scope_changes_discard_totals_and_cursors(self):
        old = dict(
            scope="other",
            repositories={
                "images": dict(
                    status="ok",
                    complete=dict(
                        started_at=self.now, measured_at=self.now, objects=99, bytes=999
                    ),
                )
            },
        )
        self.client.list_objects_v2.return_value = self.page()
        result = self.scan(old, keys=["images"])
        self.assertEqual(result["repositories"]["images"]["complete"]["objects"], 1)

    def test_summary_allowlist_staleness_and_corrupt_cache(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "private").mkdir()
            (root / "info").mkdir()
            config = dict(
                self.config, schema_version=1, repositories=[dict(key="images")]
            )
            (root / "private/repositories.yaml").write_text(yaml.safe_dump(config))
            _, keys, digest = oi.scope(root / "private/repositories.yaml")
            cache = dict(
                scope=digest,
                secret="hidden",
                repositories={
                    "images": dict(
                        status="scanning",
                        complete=dict(
                            started_at=self.now - 50,
                            measured_at=self.now,
                            objects=3,
                            bytes=10,
                            key="hidden",
                        ),
                        progress=dict(
                            started_at=self.now,
                            objects=2,
                            bytes=3,
                            token="private-token",
                        ),
                    )
                },
            )
            path = root / "info/objects-cache.json"
            oi.write_cache(path, cache)
            report = oi.summary(root, root, self.now + 1801)
            row = report["repositories"][0]
            self.assertTrue(row["stale"])
            self.assertEqual(row["objects"], 3)
            self.assertNotIn("hidden", json.dumps(report))
            self.assertNotIn("token", json.dumps(report))
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            path.write_text("{broken")
            self.assertEqual(oi.summary(root, root)["status"], "unavailable")

    def test_scope_digest_ignores_retention_but_tracks_bucket_and_repository_keys(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.yaml"
            config = dict(
                self.config,
                schema_version=1,
                repositories=[dict(key="images", enabled=False)],
            )
            path.write_text(yaml.safe_dump(config))
            digest = oi.scope(path)[2]
            config["retention"] = dict(enabled=True)
            path.write_text(yaml.safe_dump(config))
            self.assertEqual(oi.scope(path)[2], digest)
            config["bucket"] = "other-bucket"
            path.write_text(yaml.safe_dump(config))
            self.assertNotEqual(oi.scope(path)[2], digest)

    def test_client_has_bounded_timeouts_and_no_sdk_retries(self):
        with tempfile.TemporaryDirectory() as temporary:
            access, secret = Path(temporary) / "access", Path(temporary) / "secret"
            access.write_text("fixture-access")
            secret.write_text("fixture-secret")
            with patch.dict(
                os.environ,
                {"S3_ACCESS_KEY_FILE": str(access), "S3_SECRET_KEY_FILE": str(secret)},
            ):
                client = oi.client(self.config)
            self.assertEqual(client.meta.config.connect_timeout, 2)
            self.assertEqual(client.meta.config.read_timeout, 5)
            self.assertEqual(client.meta.config.retries["total_max_attempts"], 1)
            client.close()

    def test_worker_failure_marks_cache_unavailable_without_losing_last_complete(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "private").mkdir()
            (root / "info").mkdir()
            (root / ".env").write_text("DATA_ROOT=" + str(root))
            config = dict(
                self.config, schema_version=1, repositories=[dict(key="images")]
            )
            (root / "private/repositories.yaml").write_text(yaml.safe_dump(config))
            _, keys, digest = oi.scope(root / "private/repositories.yaml")
            complete = dict(
                started_at=self.now, measured_at=self.now, objects=4, bytes=10
            )
            oi.write_cache(
                root / "info/objects-cache.json",
                dict(
                    scope=digest,
                    repositories={"images": dict(status="ok", complete=complete)},
                ),
            )
            with (
                patch("sys.argv", ["object_info", "--root", str(root)]),
                patch.object(oi.os, "geteuid", return_value=0),
                patch.object(
                    oi.subprocess,
                    "run",
                    return_value=Mock(
                        returncode=1, stdout="", stderr="private-details"
                    ),
                ),
            ):
                with self.assertRaisesRegex(SystemExit, "previous totals retained"):
                    oi.main()
            report = oi.summary(root, root)
            self.assertEqual(report["repositories"][0]["status"], "unavailable")
            self.assertEqual(report["repositories"][0]["objects"], 4)
            self.assertNotIn("private-details", json.dumps(report))


if __name__ == "__main__":
    unittest.main()
