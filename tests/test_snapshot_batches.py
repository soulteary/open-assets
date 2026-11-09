"""Independent inodes can overlap; aliases, failures and durability remain safe."""

import json
import os
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from open_assets import __main__ as core, immutable, resources
import test_reference_publication as fixture


class SnapshotBatches(unittest.TestCase):
    def case(self):
        case = fixture.ReferencePublication()
        case.setUp()
        self.addCleanup(case.doCleanups)
        for index in range(96):
            p = case.source / "public/images/batch" / f"{index:03}.png"
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(f"file-{index}".encode())
        for index in range(20):
            (case.source / "public/images/batch" / f"alias-{index:03}.png").write_bytes(
                b"one immutable inode"
            )
        sha, job = case.publish()
        self.assertEqual(job["status"], "published")
        return case, sha

    def test_bounded_parallel_links_serialize_aliases_and_preserve_complete_versions(
        self,
    ):
        case, first = self.case()
        original = immutable.link
        guard = threading.Lock()
        active = maximum = alias_maximum = 0
        per_inode = {}

        def link(source, dest, proof):
            nonlocal active, maximum, alias_maximum
            key = tuple(proof["fingerprint"][:2])
            with guard:
                active += 1
                maximum = max(maximum, active)
                per_inode[key] = per_inode.get(key, 0) + 1
                alias_maximum = max(alias_maximum, per_inode[key])
            try:
                time.sleep(0.002)
                return original(source, dest, proof)
            finally:
                with guard:
                    active -= 1
                    per_inode[key] -= 1

        puts = case.store.puts
        with patch.object(immutable, "link", side_effect=link):
            second, job = case.publish()
        self.assertEqual(job["status"], "published")
        self.assertGreater(maximum, 1)
        self.assertLessEqual(maximum, 4)
        self.assertEqual(alias_maximum, 1)
        self.assertEqual(active, 0)
        self.assertEqual(case.store.puts, puts)
        manifest = case.manifest(second)
        self.assertEqual(job["incremental"]["reused_files"], len(manifest["files"]))
        aliases = [
            case.base / "releases" / second / "images/batch" / f"alias-{i:03}.png"
            for i in range(20)
        ]
        self.assertEqual(len({p.stat().st_ino for p in aliases}), 1)
        self.assertTrue(all(p.stat().st_mode & 0o222 == 0 for p in aliases))
        self.assertEqual(
            [x["path"] for x in manifest["files"]],
            sorted(x["path"] for x in manifest["files"]),
        )
        for sha in (first, second):
            core.rollback_images(case.config, "assets", sha)
        self.assertEqual((case.base / "current/batch/000.png").read_bytes(), b"file-0")

    def test_link_failure_drains_workers_without_upload_or_activation(self):
        case, first = self.case()
        original = immutable.link
        guard = threading.Lock()
        active = 0

        def link(source, dest, proof):
            nonlocal active
            with guard:
                active += 1
            try:
                time.sleep(0.002)
                if Path(dest).name == "010.png":
                    raise ValueError("source identity changed")
                return original(source, dest, proof)
            finally:
                with guard:
                    active -= 1

        puts = case.store.puts
        with patch.object(immutable, "link", side_effect=link):
            _, job = case.publish()
        self.assertEqual(job["status"], "failed")
        self.assertEqual(active, 0)
        self.assertEqual(case.store.puts, puts)
        self.assertEqual(
            (case.base / "current").readlink().as_posix(),
            "releases/" + first + "/images",
        )
        self.assertFalse(list(case.base.glob(".staging-*")))
        self.assertEqual((case.base / "current/batch/000.png").read_bytes(), b"file-0")

    def test_one_worker_preserves_serial_fallback(self):
        case, _ = self.case()
        case.config["publication"]["object_workers"] = 1
        with patch(
            "open_assets.snapshot_export.ThreadPoolExecutor",
            side_effect=AssertionError("parallel path selected"),
        ):
            _, job = case.publish()
        self.assertEqual(job["status"], "published")


class BufferedAtomic(unittest.TestCase):
    def test_json_bytes_and_atomic_failure_preserve_previous_record(self):
        with tempfile.TemporaryDirectory() as folder:
            p = Path(folder) / "record.json"
            data = {
                "unicode": "图片",
                "nested": [None, True, 1, 1.25, {"key": "value"}],
            }
            core.atomic(p, data)
            expected = json.dumps(data, ensure_ascii=False)
            self.assertEqual(p.read_text(), expected)
            with patch.object(
                core.os, "replace", side_effect=OSError("replacement refused")
            ):
                with self.assertRaises(OSError):
                    core.atomic(p, {"new": True})
            self.assertEqual(p.read_text(), expected)
            self.assertEqual(list(Path(folder).iterdir()), [p])

    def test_file_and_directory_sync_surround_replacement(self):
        with tempfile.TemporaryDirectory() as folder:
            events = []
            sync = os.fsync
            replace = os.replace

            def fsync(fd):
                events.append("sync")
                return sync(fd)

            def rename(*args):
                events.append("replace")
                return replace(*args)

            with (
                patch.object(core.os, "fsync", side_effect=fsync),
                patch.object(core.os, "replace", side_effect=rename),
            ):
                core.atomic(Path(folder) / "record.json", {"a": 1})
            self.assertEqual(events, ["sync", "replace", "sync"])


class FetchBudgetSampling(unittest.TestCase):
    def exercise(self, overflow):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            path = root / "git/assets"
            path.mkdir(parents=True)
            clock = [0.0]
            current = [0]
            scans = []
            promoted = []
            config = {
                "state_root": str(root),
                "resource_limits": {
                    "git_repository_bytes": 10,
                    "git_min_free_bytes": 1,
                },
            }

            def usage(p, limit):
                scans.append(p)
                return current[0], 0

            def run(p, args, c, **kwargs):
                if "fetch" in args:
                    for value in (0.01, 0.02, 0.09, 0.11, 0.12, 0.22):
                        clock[0] = value
                        kwargs["monitor"]()
                    temporary = args[-1].split(":", 1)[1]
                    ref = path / temporary
                    ref.parent.mkdir(parents=True, exist_ok=True)
                    ref.write_text("1" * 40 + "\n")
                    clock[0] = 0.23
                    if overflow:
                        current[0] = 100
                elif kwargs.get("input_data"):
                    promoted.append(kwargs["input_data"])
                return b""

            with (
                patch.object(resources, "usage", side_effect=usage),
                patch.object(resources, "run_git", side_effect=run),
                patch.object(resources.time, "monotonic", side_effect=lambda: clock[0]),
            ):
                if overflow:
                    with self.assertRaisesRegex(
                        resources.ResourceLimit, "git_repository_bytes"
                    ):
                        resources.fetch(
                            config, {"remote": "fixture", "branch": "main"}, path
                        )
                else:
                    resources.fetch(
                        config, {"remote": "fixture", "branch": "main"}, path
                    )
            self.assertEqual(len(promoted), 0 if overflow else 1)
            self.assertEqual(sum(p == path for p in scans), 5)

    def test_periodic_scans_coalesce_and_final_scan_is_forced(self):
        self.exercise(False)

    def test_final_limit_violation_cannot_promote_ref(self):
        self.exercise(True)
