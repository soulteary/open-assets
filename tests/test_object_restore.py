"""Real Git history with disposable files and an in-memory object store."""

import copy
import importlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from open_assets import object_restore as restore
from open_assets.object_retention import receipt_path
from open_assets.retention import archive_path

core = importlib.import_module("open_assets.__main__")


class Store:
    class exceptions:
        class NoSuchKey(Exception):
            pass

    def __init__(self):
        self.objects = {}
        self.puts = []
        self.fail_after = None

    def list_objects_v2(self, Bucket, Prefix, MaxKeys, **kwargs):
        return dict(
            Contents=[
                dict(Key=k, Size=len(v["data"]))
                for k, v in sorted(self.objects.items())
                if k.startswith(Prefix)
            ],
            IsTruncated=False,
        )

    def get_object(self, Bucket, Key):
        if Key not in self.objects:
            raise self.exceptions.NoSuchKey()
        v = self.objects[Key]
        return dict(
            Body=io.BytesIO(v["data"]),
            ContentLength=len(v["data"]),
            ContentType=v["mime"],
            CacheControl=v["cache"],
            Metadata=copy.deepcopy(v["metadata"]),
        )

    def put_object(self, Bucket, Key, Body, Metadata, ContentType, CacheControl):
        if self.fail_after is not None and len(self.puts) >= self.fail_after:
            raise OSError("fixture failure")
        data = Body.read() if hasattr(Body, "read") else Body
        self.objects[Key] = dict(
            data=data, mime=ContentType, cache=CacheControl, metadata=dict(Metadata)
        )
        self.puts.append(Key)
        return {}


class ObjectRestore(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.state = self.root / "state"
        self.images = self.root / "images"
        self.images.mkdir()
        self.config = dict(
            state_root=str(self.state),
            images_root=str(self.images),
            endpoint="http://fixture:9000",
            bucket="attachments",
            revision="fixture",
            repositories=[
                dict(
                    key="assets",
                    enabled=True,
                    remote=str(self.source),
                    branch="main",
                    source_directory="public",
                    mount_path="/",
                    max_bytes=1048576,
                )
            ],
        )
        self.git("init", "-b", "main")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@localhost.invalid")
        (self.source / "public/images").mkdir(parents=True)
        (self.source / "public/document.txt").write_bytes(b"attachment")
        (self.source / "public/empty.bin").write_bytes(b"")
        (self.source / "public/中文 +%.txt").write_bytes(b"special path")
        self.store = Store()
        self.manifests = []
        self.shas = []
        for payload in (b"image-one", b"image-two"):
            (self.source / "public/images/pic.png").write_bytes(payload)
            self.git("add", ".")
            self.git("commit", "-m", "fixture")
            sha = self.git("rev-parse", "HEAD").decode().strip()
            self.shas.append(sha)
            base = self.images / "repos/assets/releases" / sha
            base.mkdir(parents=True)
            files = core.export(
                self.config,
                self.config["repositories"][0],
                self.source / ".git",
                sha,
                base,
            )
            shutil.copytree(base / "images", base.with_name("images-" + sha))
            manifest = dict(key="assets", sha=sha, files=files, config="fixture")
            self.manifests.append(manifest)
            core.atomic(self.state / "manifests/assets" / (sha + ".json"), manifest)
            for item in files:
                self.store.objects[self.prefix(sha) + item["path"]] = dict(
                    data=(base / item["path"]).read_bytes(),
                    mime=item["content_type"],
                    cache=restore.CACHE_CONTROL,
                    metadata={"sha256": item["sha256"]},
                )
            jid = core.enqueue(self.config, "assets", sha, "fixture")
            path = self.state / "jobs" / (jid + ".json")
            job = core.read_job(path)
            job["status"] = "published"
            core.atomic(path, job)
        self.sha = self.shas[0]
        self.current = self.shas[-1]
        core.switch(self.images / "repos/assets", "images-" + self.current)
        core.atomic(self.state / "published/assets", self.manifests[-1])
        # Both completed auto and archived manual identities exist for the target.
        manual = core.enqueue(self.config, "assets", self.sha, "manual", kind="manual")
        path = self.state / "jobs" / (manual + ".json")
        job = core.read_job(path)
        job["status"] = "published"
        core.atomic(archive_path(self.config, manual), job)
        path.unlink()
        self.manual = manual
        self.original_jobs = self.job_bytes()
        self.marker = receipt_path(self.config, "assets", self.sha)
        core.atomic(
            self.marker,
            dict(
                schema_version=1,
                key="assets",
                sha=self.sha,
                status="complete",
                completed_at=time.time(),
            ),
        )
        self.original_objects = copy.deepcopy(self.store.objects)
        self.clear_objects()
        sleep = patch("open_assets.object_restore.time.sleep", lambda _: None)
        sleep.start()
        self.addCleanup(sleep.stop)

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.source), *args], stderr=subprocess.DEVNULL
        )

    def prefix(self, sha=None):
        return "repositories/assets/releases/" + (sha or self.sha) + "/"

    def clear_objects(self):
        self.store.objects = {
            k: v
            for k, v in self.store.objects.items()
            if not k.startswith(self.prefix())
        }

    def job_bytes(self):
        return {
            str(p.relative_to(self.state)): p.read_bytes()
            for root in ("jobs", "job-archive")
            for p in (self.state / root).rglob("*.json")
        }

    def run_restore(self):
        return restore.run(self.config, "assets", self.sha, self.store)

    def assert_current(self):
        self.assertEqual(
            os.readlink(self.images / "repos/assets/current"),
            "releases/images-" + self.current,
        )
        self.assertEqual(
            json.loads((self.state / "published/assets").read_text()),
            self.manifests[-1],
        )

    def test_completed_and_archived_jobs_do_not_prevent_recovery_or_change_activation(
        self,
    ):
        self.assertEqual(
            core.enqueue(self.config, "assets", self.sha, "again", kind="manual"),
            self.manual,
        )
        before = (self.state / "manifests/assets" / (self.sha + ".json")).read_bytes()
        report = self.run_restore()
        self.assertEqual(report["status"], "complete")
        self.assertEqual(report["uploaded_objects"], len(self.manifests[0]["files"]))
        self.assertEqual(report["verified_objects"], len(self.manifests[0]["files"]))
        self.assertEqual(self.store.objects, self.original_objects)
        self.assertEqual(self.job_bytes(), self.original_jobs)
        self.assert_current()
        self.assertEqual(
            before,
            (self.state / "manifests/assets" / (self.sha + ".json")).read_bytes(),
        )
        self.assertEqual(json.loads(self.marker.read_text())["status"], "restored")
        second = self.run_restore()
        self.assertEqual(second["uploaded_objects"], 0)
        self.assertEqual(second["repaired_objects"], 0)
        self.assertEqual(self.job_bytes(), self.original_jobs)
        core.rollback_images(self.config, "assets", self.sha)

    def test_all_existing_bytes_are_checked_before_any_upload(self):
        key = next(
            k
            for k in self.original_objects
            if k.startswith(self.prefix()) and self.original_objects[k]["data"]
        )
        self.store.objects[key] = copy.deepcopy(self.original_objects[key])
        self.store.objects[key]["data"] = b"x" * len(self.store.objects[key]["data"])
        with self.assertRaises(ValueError):
            self.run_restore()
        self.assertFalse(self.store.puts)
        self.assert_current()
        self.assertEqual(json.loads(self.marker.read_text())["status"], "complete")

    def test_unlisted_object_and_invalid_receipt_block_before_writing(self):
        extra = self.prefix() + "unexpected.bin"
        self.store.objects[extra] = dict(
            data=b"x", mime="application/octet-stream", cache="", metadata={}
        )
        with self.assertRaises(ValueError):
            self.run_restore()
        self.assertFalse(self.store.puts)
        self.store.objects.pop(extra)
        core.atomic(
            self.marker,
            dict(schema_version=1, key="other", sha=self.sha, status="complete"),
        )
        with self.assertRaises(ValueError):
            self.run_restore()
        self.assertFalse(self.store.puts)

    def test_metadata_repair_requires_identical_bytes_and_preserves_other_metadata(
        self,
    ):
        self.store.objects = copy.deepcopy(self.original_objects)
        key = self.prefix() + "images/pic.png"
        self.store.objects[key].update(
            mime="application/octet-stream",
            cache="no-cache",
            metadata={"custom": "preserved"},
        )
        report = self.run_restore()
        self.assertEqual(report["uploaded_objects"], 0)
        self.assertEqual(report["repaired_objects"], 1)
        self.assertEqual(self.store.objects[key]["data"], b"image-one")
        self.assertEqual(self.store.objects[key]["metadata"]["custom"], "preserved")
        self.assertEqual(self.store.objects[key]["mime"], "image/png")

    def test_interrupted_upload_is_repeatable_and_does_not_clear_deletion_marker(self):
        self.store.fail_after = 1
        with self.assertRaises(OSError):
            self.run_restore()
        report = json.loads(
            restore.report_path(self.config, "assets", self.sha).read_text()
        )
        self.assertEqual(report["status"], "failed")
        self.assertTrue(report["partial_possible"])
        self.assertEqual(report["uploaded_objects"], 1)
        self.assertEqual(json.loads(self.marker.read_text())["status"], "complete")
        self.store.fail_after = None
        report = self.run_restore()
        self.assertEqual(
            report["uploaded_objects"], len(self.manifests[0]["files"]) - 1
        )
        self.assertEqual(self.store.objects, self.original_objects)
        self.assertEqual(self.job_bytes(), self.original_jobs)
        self.assert_current()

    def test_changed_git_export_missing_manifest_and_capacity_failure_do_not_write(
        self,
    ):
        manifest = self.state / "manifests/assets" / (self.sha + ".json")
        value = json.loads(manifest.read_text())
        value["files"][0]["sha256"] = "0" * 64
        core.atomic(manifest, value)
        with self.assertRaises(ValueError):
            self.run_restore()
        core.atomic(manifest, self.manifests[0])
        with patch("shutil.disk_usage", return_value=SimpleNamespace(free=0)):
            with self.assertRaises(ValueError):
                self.run_restore()
        manifest.unlink()
        with self.assertRaises(FileNotFoundError):
            self.run_restore()
        self.assertFalse(self.store.puts)

    def test_branch_ancestry_and_disabled_repository(self):
        other = (
            self.git("commit-tree", "HEAD^{tree}", "-m", "unrelated").decode().strip()
        )
        self.git("branch", "unrelated", other)
        # Fetch the other object first so membership, not availability, is tested.
        cache = self.state / "git/assets"
        cache.mkdir(parents=True)
        subprocess.run(
            ["git", "init", "--bare", str(cache)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        core.git(
            cache,
            "fetch",
            str(self.source),
            "+refs/heads/unrelated:refs/heads/unrelated",
        )
        original = json.loads(
            (self.state / "manifests/assets" / (self.sha + ".json")).read_text()
        )
        original["sha"] = other
        core.atomic(self.state / "manifests/assets" / (other + ".json"), original)
        with self.assertRaises(subprocess.CalledProcessError):
            restore.run(self.config, "assets", other, self.store)
        self.config["repositories"][0]["enabled"] = False
        with self.assertRaises(ValueError):
            self.run_restore()
        self.assertFalse(self.store.puts)

    def test_maintenance_lock_is_busy_and_worker_loop_does_not_block_on_it(self):
        with core.lock(self.config):
            self.assertEqual(self.run_restore()["status"], "busy")
            started = time.monotonic()
            core.work_once(self.config)
            self.assertLess(time.monotonic() - started, 0.2)
        self.assertFalse(self.store.puts)

    def test_invalid_inputs_and_request_budget_are_bounded(self):
        for key, sha in [
            ("../assets", self.sha),
            ("assets", "0" * 40),
            ("assets", "bad"),
        ]:
            with self.assertRaises(ValueError):
                restore.run(self.config, key, sha, self.store)
        with patch.object(restore, "MAX_REQUESTS", 0):
            with self.assertRaises(ValueError):
                self.run_restore()
        self.assertFalse(self.store.puts)

    def test_cli_executes_restore_and_redacts_remote_error_text(self):
        output = io.StringIO()
        with (
            patch.object(core, "load", return_value=self.config),
            patch.object(core, "s3client", return_value=self.store),
            patch("sys.argv", ["open_assets", "restore-objects", "assets", self.sha]),
            patch("sys.stdout", output),
        ):
            core.main()
        self.assertEqual(json.loads(output.getvalue())["status"], "complete")
        error = io.StringIO()
        with (
            patch.object(core, "load", return_value=self.config),
            patch.object(
                restore, "run", side_effect=RuntimeError("private endpoint and secret")
            ),
            patch("sys.argv", ["open_assets", "restore-objects", "assets", self.sha]),
            patch("sys.stderr", error),
        ):
            with self.assertRaises(SystemExit) as stopped:
                core.main()
        self.assertEqual(stopped.exception.code, 1)
        self.assertNotIn("private endpoint", error.getvalue())
        self.assertEqual(
            json.loads(error.getvalue()), dict(status="failed", error="RuntimeError")
        )

    def test_final_readback_failure_keeps_barrier_until_a_verified_retry(self):
        read = self.store.get_object
        expected = len(self.manifests[0]["files"])

        def corrupt(**kwargs):
            response = read(**kwargs)
            if (
                kwargs["Key"].endswith("images/pic.png")
                and sum(k.startswith(self.prefix()) for k in self.store.objects)
                == expected
            ):
                response["Body"].close()
                response["Body"] = io.BytesIO(b"bad readback")
            return response

        with patch.object(self.store, "get_object", side_effect=corrupt):
            with self.assertRaises(ValueError):
                self.run_restore()
        self.assertEqual(json.loads(self.marker.read_text())["status"], "complete")
        report = self.run_restore()
        self.assertEqual(report["uploaded_objects"], 0)
        self.assertEqual(json.loads(self.marker.read_text())["status"], "restored")

    def test_original_manifest_mime_is_preserved_instead_of_current_guess(self):
        path = self.state / "manifests/assets" / (self.sha + ".json")
        value = json.loads(path.read_text())
        entry = next(item for item in value["files"] if item["path"] == "document.txt")
        entry["content_type"] = "application/x-original"
        core.atomic(path, value)
        self.run_restore()
        self.assertEqual(
            self.store.objects[self.prefix() + "document.txt"]["mime"],
            "application/x-original",
        )

    def test_write_intent_is_durable_before_the_first_put(self):
        put = self.store.put_object
        observed = []

        def inspect(**kwargs):
            report = json.loads(
                restore.report_path(self.config, "assets", self.sha).read_text()
            )
            observed.append(report)
            self.assertEqual(report["status"], "running")
            self.assertEqual(report["phase"], "upload")
            self.assertTrue(report["partial_possible"])
            return put(**kwargs)

        with patch.object(self.store, "put_object", side_effect=inspect):
            self.run_restore()
        self.assertTrue(observed)

    def remove_local(self, full=True, images=True):
        releases = self.images / "repos/assets/releases"
        if full:
            shutil.rmtree(releases / self.sha)
        if images:
            shutil.rmtree(releases / ("images-" + self.sha))

    def test_rebuild_both_copies_without_activation_and_repeat(self):
        self.remove_local()
        report = restore.run(
            self.config, "assets", self.sha, self.store, with_local=True
        )
        self.assertTrue(report["local_verified"])
        self.assertTrue(report["local_full_rebuilt"])
        self.assertTrue(report["local_images_rebuilt"])
        self.assertFalse(report["activated"])
        self.assert_current()
        self.assertEqual(self.job_bytes(), self.original_jobs)
        repeat = restore.run(
            self.config, "assets", self.sha, self.store, with_local=True
        )
        self.assertFalse(repeat["local_full_rebuilt"])
        self.assertFalse(repeat["local_images_rebuilt"])
        self.assertEqual(repeat["uploaded_objects"], 0)
        core.rollback_images(self.config, "assets", self.sha)

    def test_rebuild_one_missing_copy_preserves_the_existing_copy(self):
        self.remove_local(images=False)
        report = restore.run(
            self.config, "assets", self.sha, self.store, with_local=True
        )
        self.assertTrue(report["local_full_rebuilt"])
        self.assertFalse(report["local_images_rebuilt"])
        self.assert_current()
        self.remove_local(full=False)
        report = restore.run(
            self.config, "assets", self.sha, self.store, with_local=True
        )
        self.assertFalse(report["local_full_rebuilt"])
        self.assertTrue(report["local_images_rebuilt"])
        self.assert_current()

    def test_explicit_rollback_only_after_full_restore(self):
        self.remove_local()
        report = restore.run(
            self.config, "assets", self.sha, self.store, with_local=True, rollback=True
        )
        self.assertTrue(report["activated"])
        self.assertFalse(report["activation_uncertain"])
        self.assertEqual(
            os.readlink(self.images / "repos/assets/current"),
            "releases/images-" + self.sha,
        )
        self.assertEqual(
            json.loads((self.state / "published/assets").read_text()), self.manifests[0]
        )
        self.assertEqual(self.job_bytes(), self.original_jobs)

    def test_local_conflicts_links_and_extras_refuse_before_object_writes(self):
        releases = self.images / "repos/assets/releases"
        full = releases / self.sha
        image = releases / ("images-" + self.sha)
        reference = self.root / "reference"
        shutil.copytree(full, reference)
        for mutation in ("corrupt-full", "extra-image", "full-link", "image-link"):
            with self.subTest(mutation=mutation):
                for path in (full, image):
                    if path.is_symlink():
                        path.unlink()
                    else:
                        shutil.rmtree(path)
                shutil.copytree(reference, full)
                shutil.copytree(reference / "images", image)
                if mutation == "corrupt-full":
                    (full / "document.txt").write_bytes(b"conflict")
                elif mutation == "extra-image":
                    (image / "extra.bin").write_bytes(b"x")
                elif mutation == "full-link":
                    shutil.rmtree(full)
                    full.symlink_to(reference, target_is_directory=True)
                else:
                    shutil.rmtree(image)
                    image.symlink_to(reference / "images", target_is_directory=True)
                with self.assertRaises(ValueError):
                    restore.run(
                        self.config, "assets", self.sha, self.store, with_local=True
                    )
                self.assertFalse(self.store.puts)
                self.assert_current()

    def test_interrupted_pair_install_is_resumed_without_overwriting_full_copy(self):
        self.remove_local()
        rename = os.rename

        def fail_image(source, destination):
            if Path(destination).name == "images-" + self.sha:
                raise OSError("fixture second rename failure")
            return rename(source, destination)

        with patch("os.rename", side_effect=fail_image):
            with self.assertRaises(OSError):
                restore.run(
                    self.config, "assets", self.sha, self.store, with_local=True
                )
        report = json.loads(
            restore.report_path(self.config, "assets", self.sha).read_text()
        )
        self.assertTrue(report["local_partial_possible"])
        self.assertTrue(report["local_full_rebuilt"])
        self.assertEqual(json.loads(self.marker.read_text())["status"], "complete")
        self.assert_current()
        retry = restore.run(
            self.config, "assets", self.sha, self.store, with_local=True
        )
        self.assertTrue(retry["local_verified"])
        self.assertFalse(retry["local_full_rebuilt"])
        self.assertTrue(retry["local_images_rebuilt"])
        self.assertEqual(retry["uploaded_objects"], 0)
        self.assert_current()

    def test_failed_published_write_compensates_activation(self):
        self.remove_local()
        atomic = core.atomic
        failed = [False]

        def fail_after_published(path, value):
            atomic(path, value)
            if (
                Path(path) == self.state / "published/assets"
                and value.get("sha") == self.sha
                and not failed[0]
            ):
                failed[0] = True
                raise OSError("fixture post-replace failure")

        with patch.object(core, "atomic", side_effect=fail_after_published):
            with self.assertRaises(OSError):
                restore.run(
                    self.config,
                    "assets",
                    self.sha,
                    self.store,
                    with_local=True,
                    rollback=True,
                )
        self.assert_current()
        report = json.loads(
            restore.report_path(self.config, "assets", self.sha).read_text()
        )
        self.assertTrue(report["local_verified"])
        self.assertTrue(report["activation_uncertain"])
        self.assertFalse(report["activated"])
        retry = restore.run(
            self.config, "assets", self.sha, self.store, with_local=True, rollback=True
        )
        self.assertTrue(retry["activated"])

    def test_remote_readback_failure_prevents_local_install_and_activation(self):
        self.remove_local()
        get = self.store.get_object
        count = len(self.manifests[0]["files"])

        def corrupt(**kwargs):
            value = get(**kwargs)
            if (
                kwargs["Key"].endswith("images/pic.png")
                and sum(k.startswith(self.prefix()) for k in self.store.objects)
                == count
            ):
                value["Body"].close()
                value["Body"] = io.BytesIO(b"bad")
            return value

        with patch.object(self.store, "get_object", side_effect=corrupt):
            with self.assertRaises(ValueError):
                restore.run(
                    self.config,
                    "assets",
                    self.sha,
                    self.store,
                    with_local=True,
                    rollback=True,
                )
        self.assertFalse((self.images / "repos/assets/releases" / self.sha).exists())
        self.assert_current()
        self.assertEqual(json.loads(self.marker.read_text())["status"], "complete")

    def test_rollback_flag_requires_local_and_is_restricted_to_restore_command(self):
        with self.assertRaises(ValueError):
            restore.run(self.config, "assets", self.sha, self.store, rollback=True)
        for arguments in (
            ["restore-objects", "assets", self.sha, "--rollback"],
            ["status", "--with-local"],
        ):
            with (
                patch("sys.argv", ["open_assets", *arguments]),
                patch("sys.stderr", io.StringIO()),
            ):
                with self.assertRaises(SystemExit) as stopped:
                    core.main()
            self.assertEqual(stopped.exception.code, 2)
        self.remove_local()
        output = io.StringIO()
        with (
            patch.object(core, "load", return_value=self.config),
            patch.object(core, "s3client", return_value=self.store),
            patch(
                "sys.argv",
                [
                    "open_assets",
                    "restore-objects",
                    "assets",
                    self.sha,
                    "--with-local",
                    "--rollback",
                ],
            ),
            patch("sys.stdout", output),
        ):
            core.main()
        self.assertTrue(json.loads(output.getvalue())["activated"])

    def test_invalid_current_file_is_preserved_when_rollback_is_refused(self):
        self.run_restore()
        current = self.images / "repos/assets/current"
        current.unlink()
        current.write_bytes(b"keep invalid entry for inspection")
        before = (self.state / "published/assets").read_bytes()
        with self.assertRaises(ValueError):
            core.rollback_images(self.config, "assets", self.sha)
        self.assertEqual(current.read_bytes(), b"keep invalid entry for inspection")
        self.assertEqual((self.state / "published/assets").read_bytes(), before)

    def test_local_mode_reserves_three_times_quota_without_filling_disk(self):
        quota = self.config["repositories"][0]["max_bytes"]
        with patch("shutil.disk_usage", return_value=SimpleNamespace(free=quota * 2)):
            with self.assertRaises(ValueError):
                restore.run(
                    self.config, "assets", self.sha, self.store, with_local=True
                )
            self.assertFalse(self.store.puts)
            self.assertEqual(self.run_restore()["status"], "complete")


if __name__ == "__main__":
    unittest.main()
