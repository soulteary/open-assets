"""Real Git batches, bounded concurrency, and immutable snapshot isolation."""

import errno
import hashlib
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from open_assets import __main__ as core, git_batch, publication
from open_assets.resources import ResourceLimit


class Batch(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.config = dict(repositories=[dict(mount_path="/")])
        self.repo = dict(source_directory="public", mount_path="/")
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@localhost.invalid")

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.root), *args], stderr=subprocess.DEVNULL
        )

    def export(self, files, **limits):
        for name, body in files.items():
            p = self.root / "public" / name
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(body)
        self.git("add", "public")
        self.git(
            "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", "fixture"
        )
        return core.export(
            self.config,
            dict(self.repo, **limits),
            self.root / ".git",
            self.git("rev-parse", "HEAD").decode().strip(),
            self.root / "out",
        )

    def test_binary_empty_unicode_and_duplicate_blobs_use_one_batch(self):
        files = {"images/图片\t\n +%.png": os.urandom(2 * 1024**2)}
        files.update({f"images/{i}.png": b"same\0bytes\n" for i in range(30)})
        files["empty.txt"] = b""
        original = subprocess.Popen
        batches = []

        def start(command, **kwargs):
            if "--batch" in command:
                batches.append(command)
            return original(command, **kwargs)

        with patch("open_assets.git_batch.subprocess.Popen", side_effect=start):
            result = self.export(files)
        self.assertEqual(len(batches), 1)
        self.assertEqual(len(result), len(files))
        for item in result:
            self.assertEqual(
                item["sha256"], hashlib.sha256(files[item["path"]]).hexdigest()
            )
            self.assertEqual(
                (self.root / "out" / item["path"]).read_bytes(), files[item["path"]]
            )

    def test_all_paths_and_limits_checked_before_starting_batch(self):
        for name, limits, message in (
            ("images/.DS_Store", {}, "unsafe path"),
            ("images/movie.mp4", {}, "image format"),
            ("images/pic.png", {"max_file_bytes": 1}, "file limit"),
            ("images/pic.png", {"max_bytes": 1}, "publication limit"),
            ("images/pic.png", {"max_files": 0}, "publication limit"),
        ):
            with self.subTest(name=name, limits=limits):
                self.git("rm", "-rf", "--ignore-unmatch", "public")
                with patch("open_assets.git_batch.GitBatch.__enter__") as start:
                    with self.assertRaisesRegex(ValueError, message):
                        self.export({name: b"data"}, **limits)
                    start.assert_not_called()

    def test_lfs_and_active_svg_still_rejected(self):
        for body in (
            b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\n",
            b'<svg xmlns="http://www.w3.org/2000/svg"><script/></svg>',
        ):
            suffix = ".svg" if body.startswith(b"<svg") else ".png"
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                self.export({"images/pic" + suffix: body})

    def test_batch_rejects_missing_wrong_size_and_truncated_objects(self):
        oid = b"a" * 40
        original = subprocess.Popen
        for output in (
            oid + b" missing\n",
            oid + b" tree 3\nabc\n",
            oid + b" blob 4\nabc\n",
            oid + b" blob 3\nab",
            oid + b" blob 3\nabc!",
        ):

            def start(*args, **kwargs):
                return original(
                    [
                        sys.executable,
                        "-c",
                        f"import sys;sys.stdin.readline();sys.stdout.buffer.write({output!r})",
                    ],
                    **kwargs,
                )

            with (
                self.subTest(output=output),
                patch("open_assets.git_batch.subprocess.Popen", side_effect=start),
            ):
                with self.assertRaises(ValueError):
                    with git_batch.GitBatch(self.root, {}) as batch:
                        batch.write_blob(oid, 3, io.BytesIO())
                self.assertIsNotNone(batch.process.poll())

    def test_stalled_batch_times_out_and_is_reaped(self):
        original = subprocess.Popen

        def start(*args, **kwargs):
            return original(
                [sys.executable, "-c", "import time;time.sleep(20)"], **kwargs
            )

        started = time.monotonic()
        with patch("open_assets.git_batch.subprocess.Popen", side_effect=start):
            with self.assertRaisesRegex(ResourceLimit, "git_timeout_seconds"):
                with git_batch.GitBatch(
                    self.root, {"resource_limits": {"git_timeout_seconds": 1}}
                ) as batch:
                    batch.write_blob(b"a" * 40, 1, io.BytesIO())
        self.assertLess(time.monotonic() - started, 3)
        self.assertIsNotNone(batch.process.poll())


class Transfers(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.items = []
        for i in range(12):
            body = ("fixture-" + str(i)).encode()
            name = str(i) + ".png"
            (self.root / name).write_bytes(body)
            self.items.append(
                dict(
                    path=name,
                    size=len(body),
                    sha256=hashlib.sha256(body).hexdigest(),
                    content_type="image/png",
                )
            )

    class Store:
        class exceptions:
            class NoSuchKey(Exception):
                pass

        def __init__(self):
            self.objects = {}
            self.active = self.maximum = self.puts = 0
            self.lock = threading.Lock()
            self.corrupt = False

        def get_object(self, Bucket, Key):
            if Key not in self.objects:
                raise self.exceptions.NoSuchKey()
            body = self.objects[Key] + (b"bad" if self.corrupt else b"")
            return dict(Body=io.BytesIO(body))

        def put_object(self, Bucket, Key, Body, **kwargs):
            with self.lock:
                self.active += 1
                self.maximum = max(self.maximum, self.active)
                self.puts += 1
            try:
                time.sleep(0.02)
                self.objects[Key] = Body.read()
            finally:
                with self.lock:
                    self.active -= 1

    def test_parallel_upload_cap_and_replay_reads_without_reupload(self):
        store = self.Store()
        config = dict(bucket="test", publication=dict(object_workers=3))
        publication.upload(config, store, "release/", self.root, self.items)
        self.assertEqual(store.maximum, 3)
        self.assertEqual(store.puts, len(self.items))
        publication.upload(config, store, "release/", self.root, self.items)
        self.assertEqual(store.puts, len(self.items))

    def test_corrupt_readback_propagates_and_drains_workers(self):
        store = self.Store()
        store.corrupt = True
        with self.assertRaises(ValueError):
            publication.upload(
                dict(bucket="test", publication=dict(object_workers=2)),
                store,
                "release/",
                self.root,
                self.items,
            )
        self.assertEqual(store.active, 0)
        self.assertLessEqual(store.puts, 2)

    def test_worker_configuration_is_strict(self):
        for value in (0, 9, True, 2.5, "4"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                publication.policy(dict(publication=dict(object_workers=value)))
        for value in ([], {"unknown": 1}):
            with self.assertRaises(ValueError):
                publication.policy(dict(publication=value))

    def test_hardlinks_only_same_release_and_unlink_keeps_other_view(self):
        source = self.root / "0.png"
        dest = self.root / "image-view.png"
        publication.link_or_copy(source, dest)
        self.assertEqual(source.stat().st_ino, dest.stat().st_ino)
        source.unlink()
        self.assertEqual(dest.read_bytes(), b"fixture-0")

    def test_link_failure_falls_back_to_copy_and_unsafe_entries_rejected(self):
        source = self.root / "0.png"
        dest = self.root / "copied.png"
        with patch(
            "open_assets.publication.os.link",
            side_effect=OSError(errno.EXDEV, "cross-device"),
        ):
            publication.link_or_copy(source, dest)
        self.assertNotEqual(source.stat().st_ino, dest.stat().st_ino)
        self.assertEqual(source.read_bytes(), dest.read_bytes())
        link = self.root / "link.png"
        link.symlink_to(source)
        with self.assertRaises(ValueError):
            publication.link_or_copy(link, self.root / "unsafe.png")
        with patch(
            "open_assets.publication.os.link", side_effect=OSError(errno.ENOSPC, "full")
        ):
            with self.assertRaises(OSError):
                publication.link_or_copy(source, self.root / "full.png")
