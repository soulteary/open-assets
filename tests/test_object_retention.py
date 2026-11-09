"""Destructive operations use disposable records and an in-memory S3 fixture."""

import copy
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from open_assets import object_retention as r
from open_assets.object_retention_setup import iam_policy
from open_assets.object_retention_setup import setup
import test_retention as local_fixture

core = local_fixture.core


class Store:
    def __init__(self):
        self.objects = {}
        self.deleted = []
        self.versioning = {}
        self.fail_after = None

    def get_bucket_versioning(self, **kwargs):
        return self.versioning

    def list_objects_v2(self, **kwargs):
        contents = [
            dict(
                Key=k,
                Size=v["size"],
                ETag=v["etag"],
                LastModified=datetime.fromtimestamp(v["modified"], timezone.utc),
            )
            for k, v in sorted(self.objects.items())
            if k.startswith(kwargs["Prefix"])
        ]
        return dict(Contents=contents, IsTruncated=False)

    def head_object(self, **kwargs):
        v = self.objects[kwargs["Key"]]
        return dict(
            ContentLength=v["size"],
            ETag=v["etag"],
            LastModified=datetime.fromtimestamp(int(v["modified"]), timezone.utc),
            Metadata={"sha256": v["sha256"]},
        )

    def delete_object(self, **kwargs):
        if self.fail_after is not None and len(self.deleted) >= self.fail_after:
            raise RuntimeError("interrupted fixture")
        key = kwargs["Key"]
        assert self.objects[key]["etag"] == kwargs["IfMatch"]
        self.deleted.append(key)
        del self.objects[key]
        return {}


class ObjectRetention(unittest.TestCase):
    def setUp(self):
        self.fixture = local_fixture.Retention()
        self.fixture.setUp()
        self.addCleanup(self.fixture.tearDown)
        self.config = self.fixture.config
        self.config.update(
            bucket="attachments",
            endpoint="http://fixture:9000",
            object_retention=dict(
                enabled=True,
                exclusive_writer=True,
                keep_last=1,
                keep_days=7,
                min_age_hours=1,
                max_delete_versions=2,
                max_delete_objects=100,
            ),
        )
        self.store = Store()
        for sha in self.fixture.shas:
            path = self.fixture.state / "manifests/assets" / (sha + ".json")
            manifest = json.loads(path.read_text())
            for file in manifest["files"]:
                self.store.objects[r.prefix("assets", sha) + file["path"]] = dict(
                    size=file["size"],
                    sha256=file["sha256"],
                    etag='"'
                    + hashlib.md5(sha.encode(), usedforsecurity=False).hexdigest()
                    + '"',
                    modified=self.fixture.now - 40 * 86400,
                )
        self.sleep = patch("open_assets.object_retention.time.sleep", lambda _: None)
        self.sleep.start()
        self.addCleanup(self.sleep.stop)

    def run_retention(self, apply=False):
        return r.run(self.config, "assets", apply, self.store, self.fixture.now)[
            "repositories"
        ][0]

    def large_version(self, count):
        sha = self.fixture.shas[0]
        path = self.fixture.state / "manifests/assets" / (sha + ".json")
        item = json.loads(path.read_text())["files"][0]
        original = dict(
            size=40,
            sha256=item["sha256"],
            etag='"fixture"',
            modified=self.fixture.now - 40 * 86400,
        )
        for name in list(self.store.objects):
            if name.startswith(r.prefix("assets", sha)):
                del self.store.objects[name]
        r.receipt_path(self.config, "assets", sha).unlink(missing_ok=True)
        files = [dict(item, path=f"images/{i:05d}.png") for i in range(count)]
        core.atomic(path, dict(key="assets", sha=sha, files=files))
        os.utime(path, (self.fixture.now - 40 * 86400,) * 2)
        for file in files:
            self.store.objects[r.prefix("assets", sha) + file["path"]] = original.copy()
        self.config["object_retention"].update(
            max_delete_objects=1000,
            max_delete_versions=1,
            pinned_shas=self.fixture.shas[1:-1],
        )
        return sha

    def test_large_versions_make_progress_and_eventually_complete(self):
        for count in (248, 250, 1000):
            with self.subTest(objects=count):
                sha = self.large_version(count)
                self.config["object_retention"]["max_delete_objects"] = 1000
                rounds = 0
                while any(
                    k.startswith(r.prefix("assets", sha)) for k in self.store.objects
                ):
                    before = len(self.store.objects)
                    result = self.run_retention(True)
                    self.assertGreater(result["removed_objects"], 0)
                    self.assertLess(len(self.store.objects), before)
                    rounds += 1
                    self.assertLess(rounds, 20)
                self.assertEqual(
                    json.loads(r.receipt_path(self.config, "assets", sha).read_text())[
                        "status"
                    ],
                    "complete",
                )
                self.assertEqual(result["removed_versions"], 1)
                self.assertTrue(
                    all(
                        any(
                            k.startswith(r.prefix("assets", protected))
                            for k in self.store.objects
                        )
                        for protected in self.fixture.shas[1:]
                    )
                )

    def test_slow_metadata_checks_leave_time_for_deletion_progress(self):
        self.large_version(250)
        clock = [0.0]
        original = self.store.head_object

        def slow_head(**kwargs):
            clock[0] += 2
            return original(**kwargs)

        with (
            patch.object(r.time, "monotonic", side_effect=lambda: clock[0]),
            patch.object(self.store, "head_object", side_effect=slow_head),
        ):
            result = self.run_retention(True)
        self.assertGreater(result["removed_objects"], 0)
        self.assertLess(result["removed_objects"], 250)
        self.assertLess(clock[0], r.SECONDS)

    def test_policy_object_and_byte_limits_are_per_batch(self):
        sha = self.large_version(8)
        self.config["object_retention"].update(
            max_delete_objects=3, max_delete_bytes=80
        )
        result = self.run_retention(True)
        self.assertEqual(result["removed_objects"], 2)
        self.assertEqual(result["removed_bytes"], 80)
        self.assertEqual(result["removed_versions"], 0)
        self.assertEqual(result["skip_counts"]["batch-remaining"], 1)
        self.assertEqual(
            json.loads(r.receipt_path(self.config, "assets", sha).read_text())[
                "status"
            ],
            "deleting",
        )
        self.config["object_retention"]["pinned_shas"].append(sha)
        before = copy.deepcopy(self.store.objects)
        self.run_retention(True)
        self.assertEqual(before, self.store.objects)

    def test_changed_remaining_object_stops_resumption(self):
        sha = self.large_version(250)
        self.run_retention(True)
        name = next(
            k for k in self.store.objects if k.startswith(r.prefix("assets", sha))
        )
        self.store.objects[name]["etag"] = '"changed"'
        before = copy.deepcopy(self.store.objects)
        with self.assertRaisesRegex(ValueError, "receipt conflicts"):
            self.run_retention(True)
        self.assertEqual(before, self.store.objects)

    def test_preview_and_double_opt_in(self):
        before = copy.deepcopy(self.store.objects)
        self.assertTrue(self.run_retention()["candidates"])
        self.assertEqual(before, self.store.objects)
        self.config["object_retention"]["enabled"] = False
        self.assertEqual(self.run_retention(True)["removed_objects"], 0)
        self.assertEqual(before, self.store.objects)
        self.config.pop("object_retention")
        self.assertFalse(
            r.policy(self.config, self.config["repositories"][0])["enabled"]
        )

    def test_current_rollback_pins_and_all_inflight_references_survive(self):
        shas = self.fixture.shas
        core.atomic(
            self.fixture.state / "rollback-protection/assets.json",
            dict(key="assets", shas=[shas[0]]),
        )
        self.config["object_retention"]["pinned_shas"] = [shas[1]]
        self.config["retention"]["pinned_shas"] = [shas[2]]
        for sha, status in zip(
            shas[3:7], ("pending", "running", "images_prepared", "failed")
        ):
            self.fixture.job(sha, status)
        result = self.run_retention(True)
        protected = {shas[-1], *shas[:7]}
        self.assertTrue(
            all(
                any(k.startswith(r.prefix("assets", sha)) for k in self.store.objects)
                for sha in protected
            )
        )
        self.assertTrue(protected <= set(result["protected"]))

    def test_running_previous_reference_is_protected(self):
        previous = self.fixture.shas[0]
        self.fixture.job(
            self.fixture.shas[3], "running", previous="releases/images-" + previous
        )
        self.assertIn(previous, self.run_retention()["protected"])

    def test_missing_manifest_extra_objects_and_partial_uploads_are_not_deleted(self):
        a, b, c = self.fixture.shas[:3]
        (self.fixture.state / "manifests/assets" / (a + ".json")).unlink()
        self.store.objects[r.prefix("assets", b) + "extra.bin"] = next(
            iter(self.store.objects.values())
        ).copy()
        path = self.fixture.state / "manifests/assets" / (c + ".json")
        value = json.loads(path.read_text())
        value["files"].append(dict(path="missing.bin", size=1, sha256="0" * 64))
        core.atomic(path, value)
        os.utime(path, (self.fixture.now - 40 * 86400,) * 2)
        result = self.run_retention(True)
        self.assertTrue({a, b, c} <= set(result["protected"]))
        self.assertFalse(
            any(
                any(name.startswith(r.prefix("assets", sha)) for sha in (a, b, c))
                for name in self.store.deleted
            )
        )

    def test_no_current_and_versioned_buckets_fail_closed(self):
        (self.fixture.base / "current").unlink()
        (self.fixture.state / "published/assets").unlink()
        self.assertEqual(self.run_retention(True)["removed_objects"], 0)
        for status in ("Enabled", "Suspended"):
            self.store.versioning = {"Status": status}
            with self.assertRaises(ValueError):
                self.run_retention(True)

    def test_newest_count_age_and_deletion_limits(self):
        self.config["object_retention"].update(
            keep_last=3,
            max_delete_versions=1,
            max_delete_bytes=40,
            max_delete_objects=1,
        )
        result = self.run_retention(True)
        self.assertEqual(result["removed_versions"], 1)
        self.assertEqual(result["removed_objects"], 1)
        self.assertEqual(result["removed_bytes"], 40)
        self.assertTrue(set(self.fixture.shas[-3:]) <= set(result["protected"]))
        self.config["object_retention"]["keep_days"] = 100
        self.assertEqual(self.run_retention(True)["removed_objects"], 0)

    def test_head_hash_mismatch_prevents_any_version_deletion(self):
        key = next(iter(self.store.objects))
        self.store.objects[key]["sha256"] = "bad"
        with self.assertRaises(ValueError):
            self.run_retention(True)
        self.assertFalse(self.store.deleted)

    def test_partial_delete_resumes_and_finished_receipt_cannot_delete_recreated_version(
        self,
    ):
        sha = self.fixture.shas[0]
        path = self.fixture.state / "manifests/assets" / (sha + ".json")
        value = json.loads(path.read_text())
        value["files"].append(
            dict(
                path="images/two.png",
                size=40,
                sha256=hashlib.sha256(sha.encode()).hexdigest(),
            )
        )
        core.atomic(path, value)
        os.utime(path, (self.fixture.now - 40 * 86400,) * 2)
        self.store.objects[r.prefix("assets", sha) + "images/two.png"] = (
            self.store.objects[r.prefix("assets", sha) + "images/pic.png"].copy()
        )
        self.store.fail_after = 1
        with self.assertRaises(RuntimeError):
            self.run_retention(True)
        pending = r.receipt_path(self.config, "assets", sha)
        self.assertEqual(json.loads(pending.read_text())["status"], "deleting")
        self.store.fail_after = None
        self.run_retention(True)
        self.assertFalse(
            any(k.startswith(r.prefix("assets", sha)) for k in self.store.objects)
        )
        self.assertEqual(json.loads(pending.read_text())["status"], "complete")
        self.assertNotIn("objects", json.loads(pending.read_text()))
        self.assertIsNone(r.receipt(self.config, "assets", sha, "new digest"))

    def test_protected_partial_receipt_is_not_resumed(self):
        self.store.fail_after = 1
        # Fail between versions: the first receipt is complete; the next remains pending.
        with self.assertRaises(RuntimeError):
            self.run_retention(True)
        pending = [
            p
            for p in (self.fixture.state / "object-retention/assets").glob("*.json")
            if json.loads(p.read_text())["status"] == "deleting"
        ]
        self.assertEqual(len(pending), 1)
        sha = pending[0].stem
        self.config["object_retention"]["pinned_shas"] = [sha]
        self.store.fail_after = None
        before = {
            k: v
            for k, v in self.store.objects.items()
            if k.startswith(r.prefix("assets", sha))
        }
        self.run_retention(True)
        self.assertEqual(
            before,
            {
                k: v
                for k, v in self.store.objects.items()
                if k.startswith(r.prefix("assets", sha))
            },
        )

    def test_worker_queue_and_cleanup_lock_contention_are_nonblocking(self):
        for name in ("worker", "queue", "object-retention"):
            with (self.fixture.state / (name + ".lock")).open("a") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.assertEqual(
                    r.run(self.config, apply=True, s3=self.store)["status"], "busy"
                )
        self.assertFalse(self.store.deleted)

    def test_malformed_jobs_and_inconsistent_current_block(self):
        (self.fixture.state / "jobs").mkdir(exist_ok=True)
        (self.fixture.state / "jobs/bad.json").write_text("{broken")
        with self.assertRaises(ValueError):
            self.run_retention(True)
        (self.fixture.state / "jobs/bad.json").unlink()
        core.atomic(
            self.fixture.state / "published/assets",
            dict(key="assets", sha=self.fixture.shas[0]),
        )
        with self.assertRaises(ValueError):
            self.run_retention(True)
        self.assertFalse(self.store.deleted)

    def test_configuration_and_iam_scope_are_explicit(self):
        for field, value in [
            ("enabled", 1),
            ("keep_last", 0),
            ("min_age_hours", 0),
            ("max_delete_objects", 0),
            ("pinned_shas", ["bad"]),
            ("unknown", 1),
        ]:
            config = copy.deepcopy(self.config)
            config["object_retention"][field] = value
            with self.assertRaises(ValueError):
                r.policy(config, config["repositories"][0])
        policy = iam_policy(self.config)
        serialized = json.dumps(policy)
        self.assertIn("s3:DeleteObject", serialized)
        self.assertNotIn("s3:DeleteObjectVersion", serialized)
        self.config["object_retention"]["enabled"] = False
        self.assertNotIn("s3:DeleteObject", json.dumps(iam_policy(self.config)))
        self.assertNotIn("s3:PutObject", serialized)

    def test_listing_and_time_budgets_stop_before_mutation(self):
        with patch.object(r, "MAX_OBJECTS", 1):
            with self.assertRaises(ValueError):
                self.run_retention(True)
        with patch.object(r, "SECONDS", -1):
            with self.assertRaises(TimeoutError):
                self.run_retention(True)
        self.assertFalse(self.store.deleted)

    def test_manifest_and_receipt_symlinks_are_refused(self):
        directory = self.fixture.state / "manifests/assets"
        target = directory.with_name("saved")
        directory.rename(target)
        directory.symlink_to(target, target_is_directory=True)
        with self.assertRaises(ValueError):
            self.run_retention(True)
        self.assertFalse(self.store.deleted)

    def test_compaction_grants_only_configured_namespaces_then_revokes(self):
        self.config["object_retention"]["enabled"] = False
        temporary = iam_policy(self.config, compaction=True)
        deletes = [
            s for s in temporary["Statement"] if "s3:DeleteObject" in s["Action"]
        ]
        self.assertEqual(len(deletes), 1)
        self.assertTrue(all("/repositories/" in r for r in deletes[0]["Resource"]))
        self.assertNotIn("s3:PutObject", json.dumps(temporary))
        self.assertNotIn("s3:DeleteObject", json.dumps(iam_policy(self.config)))

    def test_setup_reuses_identity_and_revokes_delete_scope_when_disabled(self):
        import yaml

        private = self.fixture.root / "private"
        private.mkdir()
        (private / "tls").mkdir()
        (private / "tls/ca.crt").write_text("fixture CA")
        (private / "oc-cli").mkdir()
        config = dict(self.config, schema_version=1)
        (private / "repositories.yaml").write_text(yaml.safe_dump(config))
        calls = []
        user_info = {}

        def oc(*args):
            calls.append(args)
            if args[:3] == ("admin", "user", "info"):
                if not user_info:
                    raise RuntimeError("not found")
                return json.dumps(user_info)
            if args[:3] == ("admin", "user", "add"):
                user_info.update(userStatus="enabled", policyName="")
            if args[:3] == ("admin", "policy", "set"):
                user_info["policyName"] = "assets-object-retention"
            return "{}"

        manager = SimpleNamespace(private=private, oc=oc)
        with patch("os.chown"), patch("builtins.print"):
            setup(manager)
            secret = (private / "object-cleaner/cleaner_secret").read_bytes()
            identity = (private / "object-cleaner/identity.json").read_bytes()
            setup(manager)
            self.assertEqual(
                secret, (private / "object-cleaner/cleaner_secret").read_bytes()
            )
            self.assertEqual(
                identity, (private / "object-cleaner/identity.json").read_bytes()
            )
            self.assertEqual(sum(c[:3] == ("admin", "user", "add") for c in calls), 1)
            config["object_retention"]["enabled"] = False
            (private / "repositories.yaml").write_text(yaml.safe_dump(config))
            setup(manager)
        self.assertNotIn(
            "s3:DeleteObject", (private / "oc-cli/object-retention.json").read_text()
        )

    def test_object_deleted_version_refuses_image_rollback_until_publisher_restores(
        self,
    ):
        result = self.run_retention(True)
        sha = result["candidates"][0]["sha"]
        previous = os.readlink(self.fixture.base / "current")
        with self.assertRaisesRegex(ValueError, "objects were deleted"):
            core.rollback_images(self.config, "assets", sha)
        self.assertEqual(os.readlink(self.fixture.base / "current"), previous)
        r.restoration_record(self.config, "assets", sha, restored=True)
        core.rollback_images(self.config, "assets", sha)
        self.assertEqual(
            os.readlink(self.fixture.base / "current"), "releases/images-" + sha
        )

    def test_apply_requires_exclusive_writer_and_iam_omits_delete_without_it(self):
        self.config["object_retention"]["exclusive_writer"] = False
        self.assertTrue(self.run_retention()["candidates"])
        with self.assertRaisesRegex(ValueError, "exclusive-writer"):
            self.run_retention(True)
        self.assertFalse(self.store.deleted)
        self.assertNotIn("s3:DeleteObject", json.dumps(iam_policy(self.config)))


if __name__ == "__main__":
    unittest.main()
