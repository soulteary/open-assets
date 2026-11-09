"""Isolated recovery fixtures: only temporary directories and a fake S3 client."""

import hashlib
import io
import importlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

m = importlib.import_module("open_assets.__main__")


class Recovery(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.c = dict(
            resource_limits=dict(git_min_free_bytes=1),
            state_root=str(self.root / "state"),
            images_root=str(self.root / "images"),
            revision="fixture",
            repositories=[dict(key="fixture", enabled=True)],
        )
        self.base = self.root / "images/repos/fixture"
        self.sha = "a" * 40
        self.snapshot = self.base / "releases" / self.sha
        self.image = self.base / "releases" / ("images-" + self.sha)
        (self.snapshot / "images/nested").mkdir(parents=True)
        (self.snapshot / "images/nested/pic.png").write_bytes(b"expected-image")
        (self.snapshot / "document.txt").write_bytes(b"document")
        shutil.copytree(self.snapshot / "images", self.image)
        files = [
            dict(
                path=p.relative_to(self.snapshot).as_posix(),
                size=p.stat().st_size,
                sha256=hashlib.sha256(p.read_bytes()).hexdigest(),
            )
            for p in self.snapshot.rglob("*")
            if p.is_file()
        ]
        self.manifest = dict(key="fixture", sha=self.sha, files=files)
        m.atomic(
            self.root / "state/manifests/fixture" / (self.sha + ".json"), self.manifest
        )
        self.old = self.base / "releases" / ("images-" + "f" * 40)
        self.old.mkdir()
        (self.old / "pic.png").write_bytes(b"old")
        m.switch(self.base, "images-" + "f" * 40)
        m.atomic(self.root / "state/published/fixture", dict(sha="old"))

    def tearDown(self):
        self.tmp.cleanup()

    def assert_old(self):
        self.assertEqual(
            os.readlink(self.base / "current"), "releases/images-" + "f" * 40
        )
        self.assertEqual(
            json.loads((self.root / "state/published/fixture").read_text()),
            dict(sha="old"),
        )

    def test_rollback_checks_exact_image_copy(self):
        for mutation in (
            "missing",
            "corrupt",
            "extra",
            "link",
            "directory-link",
            "short",
        ):
            with self.subTest(mutation=mutation):
                shutil.rmtree(self.image, ignore_errors=True)
                shutil.copytree(self.snapshot / "images", self.image)
                pic = self.image / "nested/pic.png"
                if mutation == "missing":
                    shutil.rmtree(self.image)
                elif mutation == "corrupt":
                    pic.write_bytes(b"incorrectimage")
                elif mutation == "short":
                    pic.write_bytes(b"short")
                elif mutation == "extra":
                    (self.image / "unexpected.png").write_bytes(b"extra")
                elif mutation == "link":
                    pic.unlink()
                    pic.symlink_to(self.snapshot / "images/nested/pic.png")
                else:
                    shutil.rmtree(self.image / "nested")
                    (self.image / "nested").symlink_to(
                        self.snapshot / "images/nested", target_is_directory=True
                    )
                with self.assertRaises((ValueError, OSError)):
                    m.rollback_images(self.c, "fixture", self.sha)
                self.assert_old()

    def test_success_and_snapshot_identity_validation(self):
        m.rollback_images(self.c, "fixture", self.sha)
        self.assertEqual(
            (self.base / "current/nested/pic.png").read_bytes(), b"expected-image"
        )
        self.assertEqual(
            json.loads((self.root / "state/published/fixture").read_text()),
            self.manifest,
        )
        self.manifest["key"] = "wrong"
        m.atomic(
            self.root / "state/manifests/fixture" / (self.sha + ".json"), self.manifest
        )
        with self.assertRaises(ValueError):
            m.rollback_images(self.c, "fixture", self.sha)

    def test_post_activation_read_failure_restores_old_link(self):
        original = m.verify_tree
        calls = 0

        def verify(*args):
            nonlocal calls
            calls += 1
            if calls == 3:
                raise OSError("injected post-switch read failure")
            return original(*args)

        # Full snapshot, pre-switch serving tree, post-switch serving tree.
        original_switch = m.switch

        def fail_after_switch(base, target):
            original_switch(base, target)
            raise OSError("injected after link replacement")

        with patch.object(m, "switch", side_effect=fail_after_switch):
            with self.assertRaises(OSError):
                m.rollback_images(self.c, "fixture", self.sha)
        self.assert_old()
        with patch.object(m, "verify_tree", side_effect=verify):
            with self.assertRaises(OSError):
                m.rollback_images(self.c, "fixture", self.sha)
        self.assert_old()

    def test_invalid_manifest_path_and_snapshot_damage_refused(self):
        for value in (
            "../outside",
            "/absolute",
            "images/../outside",
            "images//pic.png",
        ):
            files = [dict(path=value, size=0, sha256="0" * 64)]
            with self.assertRaises(ValueError):
                m.verify_tree(self.snapshot, files)
        (self.snapshot / "document.txt").write_bytes(b"changed")
        with self.assertRaises(ValueError):
            m.rollback_images(self.c, "fixture", self.sha)
        self.assert_old()

    def test_bad_jobs_quarantined_and_healthy_jobs_continue(self):
        jobs = self.root / "state/jobs"
        jobs.mkdir()
        bad = {
            "deep.json": b"[" * 2000 + b"]" * 2000,
            "empty.json": b"",
            "invalid.json": b"{",
            "list.json": b"[]",
            "fields.json": b"{}",
            "large.json": b"x" * 65537,
        }
        for name, data in bad.items():
            (jobs / name).write_bytes(data)
        (jobs / "link.json").symlink_to(self.snapshot / "document.txt")
        os.mkfifo(jobs / "pipe.json")
        first = m.enqueue(self.c, "fixture", "b" * 40, "first")
        second = m.enqueue(self.c, "fixture", "c" * 40, "second")
        called = []

        def publish(c, j):
            called.append(j["id"])
            if j["id"] == first:
                raise ValueError("normal publication failure")
            j["status"] = "published"

        with patch.object(m, "publish", side_effect=publish):
            m.work_once(self.c)
        self.assertEqual(called, [first, second])
        self.assertEqual(m.read_job(jobs / (first + ".json"))["status"], "failed")
        self.assertEqual(m.read_job(jobs / (second + ".json"))["status"], "published")
        folders = list((self.root / "state/quarantine").iterdir())
        self.assertEqual(len(folders), len(bad) + 2)
        for folder in folders:
            name = json.loads((folder / "reason.json").read_text())["original_name"]
            if name in bad:
                self.assertEqual((folder / "record").read_bytes(), bad[name])
        with patch.object(m, "publish") as publish:
            m.work_once(self.c)
            publish.assert_not_called()

    def test_bad_sort_fields_and_identity_cannot_block(self):
        jobs = self.root / "state/jobs"
        jid = m.enqueue(self.c, "fixture", "d" * 40, "delivery")
        path = jobs / (jid + ".json")
        original = json.loads(path.read_text())
        for change in (
            dict(created="bad"),
            dict(created=float("nan")),
            dict(created=10**500),
            dict(created=None),
            dict(id="wrong"),
            dict(kind="unexpected"),
            dict(status="unexpected"),
        ):
            m.atomic(path, dict(original, **change))
            m.work_once(self.c)
            self.assertFalse(path.exists())
        self.assertEqual(len(list((self.root / "state/quarantine").iterdir())), 7)

    def test_quarantine_failure_does_not_stop_other_work(self):
        jobs = self.root / "state/jobs"
        jobs.mkdir()
        (jobs / "bad.json").write_text("{")
        jid = m.enqueue(self.c, "fixture", "e" * 40, "valid")
        with (
            patch.object(m, "quarantine_job", side_effect=OSError("disk issue")),
            patch.object(m, "publish") as publish,
        ):
            m.work_once(self.c)
            self.assertEqual(publish.call_args.args[1]["id"], jid)
        self.assertTrue((jobs / "bad.json").exists())

    def test_fetched_identity_avoids_redundant_git_but_keeps_ancestor_checks(self):
        self.c["repositories"][0]["max_bytes"] = 1
        for kind, head in (
            ("auto", self.sha),
            ("manual", self.sha),
            ("manual", "f" * 40),
        ):
            with (
                self.subTest(kind=kind, head=head),
                patch("open_assets.resources.fetch", return_value=head),
                patch.object(m, "git") as git,
                patch.object(m, "export", side_effect=ValueError("preflight barrier")),
            ):
                with self.assertRaisesRegex(ValueError, "preflight barrier"):
                    m.publish(self.c, dict(key="fixture", sha=self.sha, kind=kind))
                if head == self.sha:
                    git.assert_not_called()
                else:
                    self.assertEqual(
                        git.call_args.args[1:],
                        ("merge-base", "--is-ancestor", self.sha, head),
                    )

    def test_attachment_only_publication_fails_before_remote_side_effects(self):
        repository = self.c["repositories"][0]
        repository.update(max_bytes=1)
        with (
            patch("open_assets.resources.fetch", return_value=self.sha),
            patch.object(m, "git", return_value=self.sha.encode()),
            patch.object(
                m,
                "export",
                return_value=[dict(path="document.txt", size=1, sha256="0" * 64)],
            ),
            patch.object(m, "s3client") as client,
        ):
            with self.assertRaisesRegex(ValueError, "missing images"):
                m.publish(self.c, dict(key="fixture", sha=self.sha, kind="manual"))
            client.assert_not_called()
        self.assert_old()

    def test_real_git_publication_queue_isolation_and_rollback(self):
        source = self.root / "source"
        source.mkdir()

        def git(*args):
            return subprocess.check_output(
                ["git", "-C", str(source), *args], stderr=subprocess.DEVNULL
            )

        git("init", "-b", "main")
        git("config", "commit.gpgsign", "false")
        git("config", "user.name", "Fixture")
        git("config", "user.email", "fixture@localhost.invalid")
        (source / "public/images").mkdir(parents=True)
        (source / "public/document.txt").write_bytes(b"attachment")
        self.c["repositories"][0].update(
            remote=str(source),
            branch="main",
            source_directory="public",
            mount_path="/",
            max_bytes=1024,
        )
        self.c["bucket"] = "attachments"

        class Store:
            class exceptions:
                class NoSuchKey(Exception):
                    pass

            def __init__(self):
                self.objects = {}

            def get_object(self, Bucket, Key):
                if Key not in self.objects:
                    raise self.exceptions.NoSuchKey()
                return dict(Body=io.BytesIO(self.objects[Key]))

            def put_object(self, Bucket, Key, Body, **kwargs):
                self.objects[Key] = Body.read() if hasattr(Body, "read") else Body

        store = Store()
        versions = []
        for data in (b"image-one", b"image-two"):
            (source / "public/images/pic.png").write_bytes(data)
            git("add", ".")
            git("commit", "-m", "fixture")
            sha = git("rev-parse", "HEAD").decode().strip()
            versions.append(sha)
            jid = m.enqueue(self.c, "fixture", sha, "push-fixture")
            (self.root / "state/jobs/broken.json").write_bytes(b"{")
            with patch.object(m, "s3client", return_value=store):
                m.work_once(self.c)
            self.assertEqual(
                m.read_job(self.root / "state/jobs" / (jid + ".json"))["status"],
                "published",
            )
            self.assertEqual((self.base / "current/pic.png").read_bytes(), data)
            snapshot_file = self.base / "releases" / sha / "images/pic.png"
            serving_file = self.base / "releases" / ("images-" + sha) / "pic.png"
            self.assertEqual(snapshot_file.stat().st_ino, serving_file.stat().st_ino)
        self.assertNotEqual(
            (self.base / "releases" / versions[0] / "images/pic.png").stat().st_ino,
            (self.base / "releases" / versions[1] / "images/pic.png").stat().st_ino,
        )
        target = self.base / "releases" / ("images-" + versions[0]) / "pic.png"
        target.write_bytes(b"corrupted")
        with self.assertRaises(ValueError):
            m.rollback_images(self.c, "fixture", versions[0])
        self.assertEqual((self.base / "current/pic.png").read_bytes(), b"image-two")
        target.write_bytes(b"image-one")
        m.rollback_images(self.c, "fixture", versions[0])
        self.assertEqual((self.base / "current/pic.png").read_bytes(), b"image-one")
        m.rollback_images(self.c, "fixture", versions[1])
        self.assertEqual((self.base / "current/pic.png").read_bytes(), b"image-two")
        self.assertEqual(
            store.objects[
                "repositories/fixture/releases/" + versions[0] + "/document.txt"
            ],
            b"attachment",
        )

        # Concurrent readback failure cannot install or activate a new release.
        (source / "public/images/pic.png").write_bytes(b"image-three")
        git("add", ".")
        git("commit", "-m", "rejected readback")
        rejected = git("rev-parse", "HEAD").decode().strip()
        jid = m.enqueue(self.c, "fixture", rejected, "corrupt-fixture")
        read = store.get_object

        def corrupt(**kwargs):
            response = read(**kwargs)
            response["Body"].close()
            return dict(Body=io.BytesIO(b"corrupt"))

        with (
            patch.object(m, "s3client", return_value=store),
            patch.object(store, "get_object", side_effect=corrupt),
        ):
            m.work_once(self.c)
        self.assertEqual(
            m.read_job(self.root / "state/jobs" / (jid + ".json"))["status"],
            "failed",
        )
        self.assertFalse((self.base / "releases" / rejected).exists())
        self.assertEqual((self.base / "current/pic.png").read_bytes(), b"image-two")


if __name__ == "__main__":
    unittest.main()
