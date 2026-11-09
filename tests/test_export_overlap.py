"""Size reuse is OID-bound; read-only verification overlaps without bypassing it."""

from concurrent.futures import Future
from pathlib import Path
import threading
import unittest
from unittest.mock import patch

from open_assets import git_batch, publication
import test_reference_publication as fixture


class TreeMetadata(unittest.TestCase):
    def test_only_unknown_oids_need_metadata(self):
        known, new = b"a" * 40, b"b" * 40
        tree = (
            b"100644 blob "
            + known
            + b"\tpublic/images/a.png\0"
            + b"100644 blob "
            + new
            + b"\tpublic/images/b.png\0"
        )

        def run(path, args, config, **kwargs):
            if args[0] == "ls-tree":
                self.assertIn("-rz", args)
                return tree
            self.assertEqual(kwargs["input_data"], new + b"\n")
            return new + b" blob 3\n"

        with patch("open_assets.resources.run_git", side_effect=run):
            result = git_batch.tree_entries(
                Path("/unused"),
                {},
                "c" * 40,
                "public",
                {"a" * 40: {"item": {"size": 7}}},
            )
        self.assertEqual(
            result,
            [
                b"100644 blob " + known + b" 7\tpublic/images/a.png",
                b"100644 blob " + new + b" 3\tpublic/images/b.png",
            ],
        )

    def test_file_budget_precedes_new_metadata_queries(self):
        tree = (
            b"100644 blob "
            + b"b" * 40
            + b"\tpublic/a\0"
            + b"100644 blob "
            + b"c" * 40
            + b"\tpublic/b\0"
        )
        with patch("open_assets.resources.run_git", return_value=tree) as run:
            with self.assertRaisesRegex(ValueError, "publication limit"):
                git_batch.tree_entries(
                    Path("/unused"),
                    {},
                    "d" * 40,
                    "public",
                    {"a" * 40: {"item": {"size": 7}}},
                    max_files=1,
                )
            self.assertEqual(run.call_count, 1)

    def test_invalid_metadata_and_git_links_fail_closed(self):
        for tree, metadata in (
            (b"100644 blob " + b"b" * 40 + b"\tpublic/a\0", b"b" * 40 + b" missing\n"),
            (b"120000 blob " + b"a" * 40 + b"\tpublic/link\0", b""),
        ):
            with (
                self.subTest(tree=tree),
                patch("open_assets.resources.run_git", side_effect=[tree, metadata]),
            ):
                with self.assertRaises(ValueError):
                    git_batch.tree_entries(
                        Path("/unused"),
                        {},
                        "c" * 40,
                        "public",
                        {"a" * 40: {"item": {"size": 7}}},
                    )

    def test_metadata_stdin_is_chunked(self):
        names = [f"{i:040x}".encode() for i in range(2050)]
        calls = []

        def run(path, args, config, **kwargs):
            if args[0] == "ls-tree":
                return b"\0".join(b"100644 blob " + n + b"\tpublic/" + n for n in names)
            data = kwargs["input_data"]
            calls.append(data)
            self.assertLessEqual(len(data), 65536)
            return b"\n".join(n + b" blob 1" for n in data.splitlines()) + b"\n"

        with patch("open_assets.resources.run_git", side_effect=run):
            result = git_batch.tree_entries(
                Path("/unused"),
                {},
                "c" * 40,
                "public",
                {"f" * 40: {"item": {"size": 7}}},
            )
        self.assertEqual(len(result), 2050)
        self.assertEqual(len(calls), 3)

    def test_failed_or_different_source_plan_never_uploads(self):
        cached = {"images/a.png": {"key": "old"}}
        for expected, error in (({}, None), (cached, ValueError("missing source"))):
            future = Future()
            if error:
                future.set_exception(error)
            else:
                future.set_result({})
            with (
                self.subTest(expected=expected),
                patch.object(publication, "transfer") as transfer,
            ):
                with self.assertRaises(ValueError):
                    publication.upload(
                        {"publication": {"reference_releases": True}},
                        None,
                        "new/",
                        Path("/unused"),
                        [],
                        reuse=cached,
                        source_check=(expected, future),
                    )
                transfer.assert_not_called()


class Overlap(unittest.TestCase):
    def test_source_check_runs_during_export_and_failure_preserves_current(self):
        case = fixture.ReferencePublication()
        case.setUp()
        try:
            first, job = case.publish()
            self.assertEqual(job["status"], "published")
            (case.source / "public/images/a.png").write_bytes(b"changed")
            entered = threading.Event()
            copied = threading.Event()
            from open_assets import object_refs, incremental

            original_copy = incremental.copy_cached

            def verify(*args):
                entered.set()
                if not copied.wait(5):
                    raise AssertionError("export did not overlap")
                raise ValueError("source missing")

            def copy(*args):
                self.assertTrue(entered.wait(5))
                value = original_copy(*args)
                copied.set()
                return value

            puts = case.store.puts
            with (
                patch.object(object_refs, "verify_sources", side_effect=verify),
                patch.object(incremental, "copy_cached", side_effect=copy),
            ):
                _, job = case.publish()
            self.assertEqual(job["status"], "failed")
            self.assertEqual(case.store.puts, puts)
            self.assertEqual(
                (case.base / "current").readlink().as_posix(),
                "releases/" + first + "/images",
            )
        finally:
            case.doCleanups()
