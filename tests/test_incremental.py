"""Incremental versions preserve complete manifests, isolation and fail-closed reuse."""

import hashlib
import base64
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from open_assets import __main__ as core, incremental


class Store:
    class exceptions:
        class NoSuchKey(Exception):
            pass

    def __init__(self):
        self.objects = {}
        self.puts = self.copies = self.reads = 0
        self.bad_copy = False

    def get_object(self, Bucket, Key):
        if Key not in self.objects:
            raise self.exceptions.NoSuchKey()
        self.reads += 1
        data = self.objects[Key][0]
        return dict(
            Body=io.BytesIO(data),
            ETag='"' + hashlib.md5(data, usedforsecurity=False).hexdigest() + '"',
        )

    def put_object(self, Bucket, Key, Body, **metadata):
        self.puts += 1
        self.objects[Key] = (Body.read(), metadata)

    def copy_object(self, Bucket, Key, CopySource, CopySourceIfMatch, **metadata):
        source = self.objects[CopySource["Key"]][0]
        etag = '"' + hashlib.md5(source, usedforsecurity=False).hexdigest() + '"'
        if etag != CopySourceIfMatch:
            raise ValueError("copy precondition failed")
        self.copies += 1
        self.objects[Key] = (source, metadata)
        return dict(
            CopyObjectResult=dict(
                ETag='"wrong"' if self.bad_copy else etag,
                ChecksumSHA256=base64.b64encode(
                    hashlib.sha256(source).digest()
                ).decode(),
            )
        )

    def list_objects_v2(self, Bucket, Prefix, **kwargs):
        return dict(
            Contents=[dict(Key=k) for k in self.objects if k.startswith(Prefix)],
            IsTruncated=False,
        )


class Incremental(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / "repo"
        self.source.mkdir()
        self.git("init", "-qb", "main")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@localhost.invalid")
        self.git("config", "commit.gpgsign", "false")
        for path, body in {
            "images/a.png": b"a",
            "images/b.png": b"b",
            "document.txt": b"document",
        }.items():
            target = self.source / "public" / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(body)
        self.config = dict(
            state_root=str(self.root / "state"),
            images_root=str(self.root / "images"),
            revision="fixture",
            bucket="test",
            resource_limits=dict(git_min_free_bytes=1),
            repositories=[
                dict(
                    key="assets",
                    enabled=True,
                    remote=str(self.source),
                    branch="main",
                    source_directory="public",
                    mount_path="/",
                    max_bytes=4096,
                )
            ],
        )
        self.base = self.root / "images/repos/assets"
        self.base.mkdir(parents=True)
        self.store = Store()

    def git(self, *args):
        return (
            subprocess.check_output(
                ["git", "-C", str(self.source), *args], stderr=subprocess.DEVNULL
            )
            .decode()
            .strip()
        )

    def publish(self):
        self.git("add", "-A")
        self.git("commit", "--allow-empty", "-qm", "fixture")
        sha = self.git("rev-parse", "HEAD")
        job_id = core.enqueue(self.config, "assets", sha, "fixture")
        with patch.object(core, "s3client", return_value=self.store):
            core.work_once(self.config)
        job = core.read_job(
            Path(self.config["state_root"]) / "jobs" / (job_id + ".json")
        )
        return sha, job

    def test_changed_only_bytes_and_independent_complete_versions(self):
        first, job = self.publish()
        self.assertEqual(job["status"], "published")
        self.assertEqual(self.store.puts, 3)
        self.assertEqual(job["incremental"]["reused_files"], 0)
        (self.source / "public/images/a.png").write_bytes(b"updated")
        second, job = self.publish()
        self.assertEqual(job["status"], "published")
        self.assertEqual(
            job["incremental"],
            dict(
                reused_files=2,
                changed_files=1,
                object_transfers=dict(uploaded=1, copied=2),
            ),
        )
        self.assertEqual(self.store.puts, 4)
        self.assertEqual(self.store.copies, 2)
        # Three initial reads and only the changed file read back, no unchanged GETs.
        self.assertEqual(self.store.reads, 4)
        self.assertNotEqual(
            (self.base / "releases" / first / "images/b.png").stat().st_ino,
            (self.base / "releases" / second / "images/b.png").stat().st_ino,
        )
        for sha in (first, second):
            prefix = f"repositories/assets/releases/{sha}/"
            self.assertEqual(
                len([k for k in self.store.objects if k.startswith(prefix)]), 3
            )
        core.rollback_images(self.config, "assets", first)
        self.assertEqual((self.base / "current/a.png").read_bytes(), b"a")
        core.rollback_images(self.config, "assets", second)
        self.assertEqual((self.base / "current/a.png").read_bytes(), b"updated")

    def test_rename_delete_and_mime_replacement(self):
        first, _ = self.publish()
        (self.source / "public/images/b.png").rename(
            self.source / "public/images/renamed.jpg"
        )
        (self.source / "public/document.txt").unlink()
        second, job = self.publish()
        self.assertEqual(job["status"], "published")
        self.assertEqual(job["incremental"]["reused_files"], 2)
        key = f"repositories/assets/releases/{second}/images/renamed.jpg"
        self.assertEqual(self.store.objects[key][1]["ContentType"], "image/jpeg")
        self.assertFalse((self.base / "current/b.png").exists())
        self.assertTrue((self.base / "releases" / first / "images/b.png").exists())

    def test_changed_local_identity_forces_git_read(self):
        first, _ = self.publish()
        (self.base / "releases" / first / "images/b.png").write_bytes(b"corrupt")
        second, job = self.publish()
        self.assertEqual(job["status"], "published")
        self.assertEqual(job["incremental"]["changed_files"], 1)
        self.assertEqual((self.base / "current/b.png").read_bytes(), b"b")

    def test_remote_source_conflict_and_bad_copy_do_not_activate(self):
        for bad_copy in (False, True):
            with self.subTest(bad_copy=bad_copy):
                first, _ = self.publish()
                if bad_copy:
                    self.store.bad_copy = True
                else:
                    key = f"repositories/assets/releases/{first}/images/b.png"
                    self.store.objects[key] = (b"changed", {})
                _, job = self.publish()
                self.assertEqual(job["status"], "failed")
                self.assertEqual(
                    (self.base / "current").readlink().as_posix(),
                    "releases/images-" + first,
                )
                self.store.bad_copy = False
                # Reset source evidence for the next subcase by invalidating the seal.
                (Path(self.config["state_root"]) / "incremental/assets.json").unlink()

    def test_invalid_seal_or_changed_validation_config_falls_back(self):
        self.publish()
        seal = Path(self.config["state_root"]) / "incremental/assets.json"
        seal.write_text("{")
        _, job = self.publish()
        self.assertEqual(job["incremental"]["reused_files"], 0)
        self.config["revision"] = "changed"
        _, job = self.publish()
        self.assertEqual(job["incremental"]["reused_files"], 0)

    def test_rename_into_svg_cannot_bypass_svg_validation(self):
        (self.source / "public/active.txt").write_bytes(
            b'<svg xmlns="http://www.w3.org/2000/svg"><script/></svg>'
        )
        first, _ = self.publish()
        puts = self.store.puts
        (self.source / "public/active.txt").rename(
            self.source / "public/images/active.svg"
        )
        _, job = self.publish()
        self.assertEqual(job["status"], "failed")
        self.assertEqual(self.store.puts, puts)
        self.assertEqual(
            (self.base / "current").readlink().as_posix(), "releases/images-" + first
        )

    def test_seal_is_optional_and_sha_is_bound_to_manifest(self):
        self.publish()
        seal_path = Path(self.config["state_root"]) / "incremental/assets.json"
        seal = json.loads(seal_path.read_text())
        seal["manifest"] = "0" * 64
        seal_path.write_text(json.dumps(seal))
        context = incremental.context(self.config, self.config["repositories"][0])
        self.assertFalse(context["sources"])

    def test_compatible_copy_without_checksum_retains_full_readback(self):
        self.publish()
        original = self.store.copy_object

        def without_checksum(**kwargs):
            result = original(**kwargs)
            result["CopyObjectResult"].pop("ChecksumSHA256")
            return result

        with patch.object(self.store, "copy_object", side_effect=without_checksum):
            _, job = self.publish()
        self.assertEqual(job["status"], "published")
        self.assertEqual(job["incremental"]["object_transfers"], {"copied-readback": 3})
        self.assertEqual(self.store.reads, 6)

    def test_copy_sha256_mismatch_is_rejected_even_with_matching_etag(self):
        first, _ = self.publish()
        original = self.store.copy_object

        def bad_checksum(**kwargs):
            result = original(**kwargs)
            result["CopyObjectResult"]["ChecksumSHA256"] = "incorrect"
            return result

        with patch.object(self.store, "copy_object", side_effect=bad_checksum):
            _, job = self.publish()
        self.assertEqual(job["status"], "failed")
        self.assertEqual(
            (self.base / "current").readlink().as_posix(), "releases/images-" + first
        )
