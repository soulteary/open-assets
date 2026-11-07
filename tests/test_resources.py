"""Exercise admission refusal, Git budgets, and fixed-size object reads."""

import hashlib
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import concurrent.futures
import time

from open_assets import __main__ as core
from open_assets import resources as limits


class Resources(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.c = dict(
            resource_limits=dict(git_min_free_bytes=1),
            state_root=str(self.root / "state"),
            revision="fixture",
            repositories=[dict(key="assets", enabled=True)],
        )

    def test_queue_full_still_deduplicates_and_counts_archives(self):
        self.c["resource_limits"] = dict(queue_active_records=1, queue_total_records=2)
        first = core.enqueue(self.c, "assets", "a" * 40, "one")
        self.assertEqual(core.enqueue(self.c, "assets", "a" * 40, "two"), first)
        with self.assertRaisesRegex(limits.ResourceLimit, "queue_active_records"):
            core.enqueue(self.c, "assets", "b" * 40, "two")
        active = self.root / "state/jobs" / (first + ".json")
        archive = self.root / "state/job-archive/aa/one.json"
        archive.parent.mkdir(parents=True)
        active.rename(archive)
        core.enqueue(self.c, "assets", "b" * 40, "two")
        self.c["resource_limits"]["queue_active_records"] = 2
        with self.assertRaisesRegex(limits.ResourceLimit, "queue_total_records"):
            core.enqueue(self.c, "assets", "c" * 40, "three")

    def test_concurrent_admission_cannot_overrun_limit(self):
        self.c["resource_limits"] = dict(queue_active_records=3, queue_total_records=3)

        def submit(number):
            try:
                return core.enqueue(self.c, "assets", f"{number:040x}", "concurrent")
            except limits.ResourceLimit:
                return None

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            result = list(pool.map(submit, range(1, 17)))
        self.assertEqual(sum(value is not None for value in result), 3)
        self.assertEqual(len(list((self.root / "state/jobs").glob("*.json"))), 3)

    def test_git_timeout_stops_process(self):
        started = time.monotonic()
        with self.assertRaisesRegex(limits.ResourceLimit, "git_timeout_seconds"):
            limits.run_git(
                self.root,
                ["-c", "alias.pause=!sleep 10", "pause"],
                dict(resource_limits=dict(git_timeout_seconds=1)),
            )
        self.assertLess(time.monotonic() - started, 3)

    def test_reserved_bytes_and_quarantine_block_admission(self):
        self.c["resource_limits"] = dict(queue_reserved_bytes=65536)
        folder = self.root / "state/quarantine/bad"
        folder.mkdir(parents=True)
        (folder / "record").write_text("{")
        with self.assertRaisesRegex(limits.ResourceLimit, "queue_reserved_bytes"):
            core.enqueue(self.c, "assets", "a" * 40, "test")
        self.assertEqual((folder / "record").read_text(), "{")

    def test_queue_configuration_and_unsafe_entry(self):
        with self.assertRaises(ValueError):
            limits.policy(dict(resource_limits=dict(queue_total_records=0)))
        with self.assertRaises(ValueError):
            limits.policy(dict(resource_limits=dict(unknown=1)))
        folder = self.root / "state/jobs"
        folder.mkdir(parents=True)
        (folder / "unsafe").symlink_to(self.root)
        self.c["resource_limits"] = dict(queue_reserved_bytes=65536)
        with self.assertRaisesRegex(limits.ResourceLimit, "queue_reserved_bytes"):
            core.enqueue(self.c, "assets", "a" * 40, "test")

    def test_streaming_body_bounded_and_closed_on_all_paths(self):
        class Body(io.BytesIO):
            def read(self, size=-1):
                if size < 0 or size > 1024**2:
                    raise AssertionError("unbounded read")
                return super().read(size)

        data = b"x" * (3 * 1024**2)
        item = dict(size=len(data), sha256=hashlib.sha256(data).hexdigest())
        for value, success in ((data, True), (data[:-1], False), (data + b"x", False)):
            body = Body(value)
            if success:
                limits.verify_body(body, item)
            else:
                with self.assertRaises(ValueError):
                    limits.verify_body(body, item)
            self.assertTrue(body.closed)

    def test_git_output_limit_and_fetch_preserves_source_on_refusal(self):
        source = self.root / "source"
        subprocess.run(["git", "init", "-q", str(source)], check=True)

        def git(*args):
            return subprocess.check_output(
                ["git", "-C", str(source), *args], stderr=subprocess.DEVNULL
            )

        git("config", "user.name", "Fixture")
        git("config", "user.email", "fixture@localhost.invalid")
        (source / "file").write_text("a" * 4096)
        git("add", ".")
        git("-c", "commit.gpgsign=false", "commit", "-qm", "fixture")
        branch = git("branch", "--show-current").decode().strip()
        path = self.root / "state/git/assets"
        path.parent.mkdir(parents=True)
        subprocess.run(["git", "init", "--bare", "-q", str(path)], check=True)
        limits.fetch(self.c, dict(remote=str(source), branch=branch), path)
        head = core.git(path, "rev-parse", "refs/heads/source")
        self.c["resource_limits"] = dict(git_repository_bytes=1)
        with self.assertRaisesRegex(limits.ResourceLimit, "git_repository_bytes"):
            limits.fetch(self.c, dict(remote=str(source), branch=branch), path)
        self.assertEqual(core.git(path, "rev-parse", "refs/heads/source"), head)
        sha = git("rev-parse", "HEAD:file").decode().strip()
        with self.assertRaisesRegex(limits.ResourceLimit, "git_output_bytes"):
            limits.run_git(
                path,
                ["cat-file", "blob", sha],
                dict(resource_limits=dict(git_output_bytes=128)),
            )

        # Refuse an actual transfer, not only an already oversized cache.
        (source / "file").write_bytes(os.urandom(65536))
        git("add", ".")
        git("-c", "commit.gpgsign=false", "commit", "-qm", "larger fixture")
        self.c["resource_limits"] = dict(git_fetch_bytes=8192, git_min_free_bytes=1)
        with self.assertRaises((limits.ResourceLimit, subprocess.CalledProcessError)):
            limits.fetch(self.c, dict(remote=str(source), branch=branch), path)
        self.assertEqual(core.git(path, "rev-parse", "refs/heads/source"), head)


if __name__ == "__main__":
    unittest.main()
