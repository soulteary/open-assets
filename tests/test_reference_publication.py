"""Reference releases avoid per-unchanged-object requests without losing proofs."""

import datetime
import errno
import hashlib
import json
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

from open_assets import __main__ as core, immutable, object_refs, object_retention
from open_assets import incremental
import test_incremental as fixture


class ReferenceStore(fixture.Store):
    def __init__(self):
        super().__init__()
        self.list_calls = 0

    def list_objects_v2(self, Bucket, Prefix, **kwargs):
        self.list_calls += 1
        return dict(
            Contents=[
                dict(
                    Key=k,
                    Size=len(value[0]),
                    ETag='"'
                    + hashlib.md5(value[0], usedforsecurity=False).hexdigest()
                    + '"',
                    LastModified=datetime.datetime.fromtimestamp(
                        123, datetime.timezone.utc
                    ),
                )
                for k, value in self.objects.items()
                if k.startswith(Prefix)
            ],
            IsTruncated=False,
        )

    def copy_object(self, **kwargs):
        raise AssertionError("unchanged object must not be copied")

    def get_bucket_versioning(self, **kwargs):
        return {}


class ReferencePublication(unittest.TestCase):
    git = fixture.Incremental.git
    publish = fixture.Incremental.publish

    def setUp(self):
        fixture.Incremental.setUp(self)
        self.config["publication"] = dict(reference_releases=True)
        self.store = ReferenceStore()

    def manifest(self, sha):
        return json.loads(
            (
                Path(self.config["state_root"]) / "manifests/assets" / (sha + ".json")
            ).read_text()
        )

    def test_delta_has_no_copy_or_unchanged_get_and_keeps_original_url_resolution(self):
        first, job = self.publish()
        self.assertEqual(job["status"], "published")
        (self.source / "public/images/a.png").write_bytes(b"updated")
        second, job = self.publish()
        self.assertEqual(job["status"], "published")
        self.assertEqual(
            job["incremental"]["object_transfers"],
            dict(uploaded=1, referenced=2, source_list_requests=1, source_ranges=1),
        )
        self.assertEqual(self.store.puts, 4)
        self.assertEqual(self.store.reads, 4)
        self.assertEqual(self.store.list_calls, 1)
        manifest = self.manifest(second)
        self.assertEqual(
            manifest["object_sources"]["images/b.png"]["key"],
            f"repositories/assets/releases/{first}/images/b.png",
        )
        self.assertEqual(
            (self.base / "releases" / first / "images/b.png").stat().st_ino,
            (self.base / "releases" / second / "images/b.png").stat().st_ino,
        )
        self.assertNotEqual(
            (self.base / "releases" / first / "images/a.png").stat().st_ino,
            (self.base / "releases" / second / "images/a.png").stat().st_ino,
        )
        self.assertEqual((self.base / "current/b.png").stat().st_mode & 0o222, 0)
        core.rollback_images(self.config, "assets", first)
        self.assertEqual((self.base / "current/a.png").read_bytes(), b"a")
        core.rollback_images(self.config, "assets", second)
        self.assertEqual((self.base / "current/a.png").read_bytes(), b"updated")

    def test_rename_keeps_direct_origin_and_does_not_build_reference_chains(self):
        first, _ = self.publish()
        (self.source / "public/images/b.png").rename(
            self.source / "public/images/new.jpg"
        )
        second, job = self.publish()
        self.assertEqual(job["status"], "published")
        third, job = self.publish()
        self.assertEqual(job["status"], "published")
        value = self.manifest(third)["object_sources"]["images/new.jpg"]
        self.assertEqual(
            value,
            dict(
                key=f"repositories/assets/releases/{first}/images/b.png",
                content_type="image/png",
            ),
        )
        self.assertNotIn(second, value["key"])
        self.assertEqual(self.store.puts, 3)

    def test_bulk_source_conflict_fails_before_new_writes_or_activation(self):
        first, _ = self.publish()
        key = f"repositories/assets/releases/{first}/images/b.png"
        self.store.objects[key] = (b"altered", {})
        (self.source / "public/images/a.png").write_bytes(b"new")
        _, job = self.publish()
        self.assertEqual(job["status"], "failed")
        self.assertEqual(self.store.puts, 3)
        self.assertEqual(
            (self.base / "current").readlink().as_posix(),
            "releases/" + first + "/images",
        )

    def test_activation_checks_new_inode_after_switch_and_restores_previous(self):
        first, _ = self.publish()
        (self.source / "public/images/a.png").write_bytes(b"new")
        original = core.switch

        def damage(base, target):
            original(base, target)
            path = base / "current/a.png"
            path.chmod(0o644)
            path.write_bytes(b"bad")

        with patch.object(core, "switch", side_effect=damage):
            _, job = self.publish()
        self.assertEqual(job["status"], "failed")
        self.assertEqual(
            (self.base / "current").readlink().as_posix(),
            "releases/" + first + "/images",
        )
        self.assertEqual((self.base / "current/a.png").read_bytes(), b"a")

    def test_unavailable_link_falls_back_to_verified_independent_copy(self):
        self.publish()
        with patch(
            "open_assets.immutable.os.link", side_effect=OSError(errno.EXDEV, "fixture")
        ):
            _, job = self.publish()
        self.assertEqual(job["status"], "published")
        self.assertEqual((self.base / "current/a.png").read_bytes(), b"a")

    def test_origins_are_protected_and_retention_counts_only_owned_objects(self):
        first, _ = self.publish()
        (self.source / "public/images/a.png").write_bytes(b"updated")
        second, _ = self.publish()
        origins, versions = object_refs.reference_inventory(
            self.config, "assets", time.monotonic() + 10
        )
        self.assertIn(first, origins)
        self.assertIn(second, versions)
        owned, _ = object_retention.manifest(self.config, "assets", second)
        self.assertEqual(
            set(owned), {f"repositories/assets/releases/{second}/images/a.png"}
        )
        marker = (
            Path(self.config["state_root"])
            / "object-retention/assets"
            / (second + ".json")
        )
        core.atomic(marker, dict(key="assets", sha=second, status="complete"))
        origins, _ = object_refs.reference_inventory(
            self.config, "assets", time.monotonic() + 10
        )
        self.assertNotIn(first, origins)

    def test_invalid_reference_cannot_escape_project_or_manifest(self):
        first, _ = self.publish()
        manifest = self.manifest(first)
        for value in (
            f"repositories/foreign/releases/{first}/images/a.png",
            f"repositories/assets/releases/{first}/../secret",
        ):
            manifest["object_sources"] = {
                "images/a.png": dict(key=value, content_type="image/png")
            }
            with self.assertRaises(ValueError):
                object_refs.validate(manifest)

    def test_zero_owned_release_is_retired_before_its_origins_can_be_collected(self):
        first, _ = self.publish()
        second, _ = self.publish()
        for path in (self.source / "public").rglob("*"):
            if path.is_file():
                path.write_bytes(path.read_bytes() + b"updated")
        third, _ = self.publish()
        for index, sha in enumerate((first, second, third)):
            aged = time.time() - (3 - index) * 86400
            for path in (
                self.base / "releases" / sha,
                self.base / "releases" / sha / "images",
                Path(self.config["state_root"]) / "manifests/assets" / (sha + ".json"),
            ):
                os.utime(path, (aged, aged))
        self.config.update(
            endpoint="https://store.invalid",
            object_retention=dict(
                enabled=True,
                exclusive_writer=True,
                keep_last=1,
                keep_days=0,
                min_age_hours=1,
            ),
        )
        with (
            patch.object(
                object_retention.local,
                "protected_references",
                side_effect=lambda *args: (third, {third: ["current"]}),
            ),
            patch.object(object_retention.time, "sleep", return_value=None),
        ):
            budget = object_retention.Budget(self.store)
            plan = object_retention.plan(
                self.config,
                self.config["repositories"][0],
                [],
                budget,
                time.time() + 40 * 86400,
            )
            self.assertIn("object-reference", plan["protected"][first])
            logical = next(x for x in plan["candidates"] if x["sha"] == second)
            self.assertEqual(logical["objects"], {})
            result = object_retention.remove(
                self.config, self.config["repositories"][0], logical, budget
            )
            self.assertEqual(result["status"], "complete")
            plan = object_retention.plan(
                self.config,
                self.config["repositories"][0],
                [],
                object_retention.Budget(self.store),
                time.time() + 40 * 86400,
            )
            self.assertNotIn(first, plan["protected"])
            self.assertIn(first, {x["sha"] for x in plan["candidates"]})

    def test_idle_audit_renews_bytes_but_not_merely_reused_receipts(self):
        self.publish()
        path = Path(self.config["state_root"]) / "incremental/assets.json"
        seal = json.loads(path.read_text())
        old = time.time() - 2000
        for record in seal["records"].values():
            record["immutable"]["verified_at"] = old
        path.write_text(json.dumps(seal))
        with patch.object(incremental, "_audit_next", 0):
            incremental.audit_idle(self.config)
        updated = json.loads(path.read_text())
        self.assertTrue(
            all(
                r["immutable"]["verified_at"] > old for r in updated["records"].values()
            )
        )

    def test_expired_proof_cannot_skip_byte_validation(self):
        self.publish()
        path = Path(self.config["state_root"]) / "incremental/assets.json"
        seal = json.loads(path.read_text())
        for record in seal["records"].values():
            record["immutable"]["verified_at"] = time.time() - 7200
        path.write_text(json.dumps(seal))
        _, job = self.publish()
        self.assertEqual(job["incremental"]["reused_files"], 0)
        self.assertEqual(job["incremental"]["object_transfers"]["uploaded"], 3)


class Immutable(unittest.TestCase):
    def test_mutated_file_rejects_stat_proof_and_writable_file_is_not_trusted(self):
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory) / "file"
            p.write_bytes(b"good")
            item = dict(size=4, sha256=hashlib.sha256(b"good").hexdigest())
            proof = immutable.record(p, item)
            self.assertTrue(immutable.matches(p, item, proof))
            p.chmod(0o644)
            self.assertFalse(immutable.matches(p, item, proof))
            p.write_bytes(b"evil")
            with self.assertRaises(ValueError):
                immutable.link(p, Path(directory) / "new", proof)
