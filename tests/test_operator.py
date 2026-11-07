"""Operator help, discovery and dispatch never initialize or disclose secrets."""

import copy
import io
import json
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

import yaml

from open_assets import __main__ as core, cli, operator


class Operator(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        (self.root / "private").mkdir()
        self.config = dict(
            schema_version=1,
            state_root="/state",
            images_root="/images",
            bucket="attachments",
            endpoint="https://storage.internal",
            repositories=[
                dict(
                    key="assets",
                    name="content/assets",
                    repository_id=1,
                    remote="ssh://git@gitea:2222/content/assets.git",
                    branch="main",
                    source_directory="public",
                    mount_path="/project/",
                    enabled=True,
                    serve=True,
                )
            ],
        )
        self.write()
        (self.root / "private/domains.json").write_text(
            json.dumps(dict(img="img-node-name.lab.dev", oss="oss-node-name.lab.dev"))
        )

    def write(self):
        (self.root / "private/repositories.yaml").write_text(
            yaml.safe_dump(self.config)
        )

    def test_every_operation_has_own_help_without_loading_configuration(self):
        for command in cli.DESCRIPTIONS:
            with (
                patch("sys.argv", ["open_assets", command, "--help"]),
                patch.object(core, "load", side_effect=AssertionError("must not load")),
                patch("sys.stdout", io.StringIO()) as output,
            ):
                with self.assertRaises(SystemExit) as stopped:
                    core.main()
                self.assertEqual(stopped.exception.code, 0)
                self.assertIn(cli.DESCRIPTIONS[command], output.getvalue())

    def test_missing_and_misplaced_arguments_fail_before_configuration(self):
        cases = [
            ["publish"],
            ["publish", "assets"],
            ["retry"],
            ["rollback-images", "assets"],
            ["status", "oops"],
            ["status", "--sha", "short"],
            ["publish", "assets", "a" * 40, "--apply"],
            ["restore-audit-archive", "assets", "--keep-last", "0"],
            ["close-restore", "assets", "a" * 40],
            ["restore-objects", "assets", "a" * 40, "--rollback"],
        ]
        for args in cases:
            with (
                patch("sys.argv", ["open_assets", *args]),
                patch.object(core, "load", side_effect=AssertionError("must not load")),
                patch("sys.stderr", io.StringIO()) as output,
            ):
                with self.assertRaises(SystemExit) as stopped:
                    core.main()
                self.assertEqual(stopped.exception.code, 2)
                self.assertNotIn("Traceback", output.getvalue())

    def test_repo_list_requires_no_admin_credentials_or_docker(self):
        with (
            patch.object(
                operator.subprocess,
                "run",
                side_effect=AssertionError("no external calls"),
            ),
            patch("sys.stdout", io.StringIO()) as output,
        ):
            self.assertEqual(
                operator.main(["--root", str(self.root), "repo", "list", "--json"]), 0
            )
        row = json.loads(output.getvalue())["repositories"][0]
        self.assertEqual(row["key"], "assets")
        self.assertNotIn("remote", row)
        self.assertFalse((self.root / "private/admin_password").exists())

    def test_link_generation_encodes_paths_once_and_does_not_claim_publication(self):
        with patch.object(
            operator.subprocess, "run", side_effect=AssertionError("no external calls")
        ):
            result = operator.links(
                self.root, "assets", "a" * 40, "public/images/中文 +%.png"
            )
        self.assertEqual(
            result["image"],
            "https://img-node-name.lab.dev/project/%E4%B8%AD%E6%96%87%20%2B%25.png",
        )
        self.assertTrue(
            result["attachment"].endswith("/images/%E4%B8%AD%E6%96%87%20%2B%25.png")
        )
        self.assertFalse(result["verified"])
        self.assertIsNone(
            operator.links(self.root, "assets", "a" * 40, "documents/manual.pdf")[
                "image"
            ]
        )
        for name in (
            "../secret",
            "/etc/passwd",
            "images/../secret",
            "images//file",
            "images/.secret",
        ):
            with self.assertRaises(ValueError):
                operator.links(self.root, "assets", "a" * 40, name)

    def test_config_flags_must_be_boolean_not_quoted_false(self):
        for flag in ("enabled", "serve"):
            c = copy.deepcopy(self.config)
            c["repositories"][0][flag] = "false"
            (self.root / "private/repositories.yaml").write_text(yaml.safe_dump(c))
            with self.assertRaises(ValueError):
                core.load(self.root / "private/repositories.yaml")

    def test_forwarding_uses_fixed_arguments_and_preserves_runtime_exit(self):
        result = subprocess.CompletedProcess([], 2)
        with patch.object(operator.subprocess, "run", return_value=result) as run:
            code = operator.main(
                [
                    "--root",
                    str(self.root),
                    "restore-objects",
                    "assets",
                    "a" * 40,
                    "--with-local",
                ]
            )
        self.assertEqual(code, 2)
        args = run.call_args.args[0]
        self.assertEqual(
            args[-5:],
            ["open_assets", "restore-objects", "assets", "a" * 40, "--with-local"],
        )
        self.assertNotIn("shell", run.call_args.kwargs)
        self.assertIn("publisher", args)
        self.assertNotIn("--rollback", args)

    def test_administrative_ensure_compatibility_and_root_forwarding(self):
        from open_assets import admin

        with patch.object(admin, "main", return_value=0) as run:
            self.assertEqual(
                operator.main(
                    [
                        "--root",
                        str(self.root),
                        "repo",
                        "ensure",
                        "content/new",
                        "--key",
                        "new",
                        "--mount-path",
                        "/new/",
                    ]
                ),
                0,
            )
        self.assertEqual(
            run.call_args.args[0],
            [
                "--root",
                str(self.root),
                "repo",
                "ensure",
                "content/new",
                "--key",
                "new",
                "--mount-path",
                "/new/",
                "--branch",
                "main",
            ],
        )

    def test_doctor_uses_allowlisted_fields_and_no_mutation(self):
        data = self.root / "data"
        (data / "info").mkdir(parents=True)
        (self.root / ".env").write_text("DATA_ROOT=" + str(data) + "\n")
        (data / "info/snapshot.json").write_text(
            json.dumps(dict(schema_version=1, collected_at=time.time()))
        )
        containers = [
            dict(Service=s, State="running", Health="healthy", Env="secret")
            for s in operator.SERVICES
        ]

        def inspect(args, **kwargs):
            return json.dumps(containers) if args[0] == "docker" else "active\n"

        before = {p: p.read_bytes() for p in self.root.rglob("*") if p.is_file()}
        with patch.object(operator, "inspect_command", side_effect=inspect) as command:
            result = operator.doctor(self.root)
        self.assertEqual(result["status"], "ok")
        self.assertNotIn("secret", json.dumps(result))
        self.assertEqual(
            before, {p: p.read_bytes() for p in self.root.rglob("*") if p.is_file()}
        )
        self.assertTrue(
            all(
                "restart" not in c.args[0] and "start" not in c.args[0]
                for c in command.call_args_list
            )
        )

    def test_doctor_missing_deployment_is_actionable_not_a_traceback(self):
        (self.root / "private/repositories.yaml").write_text("{")
        with patch.object(
            operator, "inspect_command", side_effect=OSError("private token")
        ):
            result = operator.doctor(self.root)
        self.assertEqual(result["status"], "attention")
        self.assertNotIn("private token", json.dumps(result))
        self.assertTrue(all(c["hint"] for c in result["checks"]))

    def test_host_help_and_disabled_daemon_commands_need_no_privileges(self):
        for args in (
            ["--help"],
            ["restore-audit-archive", "--help"],
            ["repo", "list", "--help"],
        ):
            with (
                patch("sys.stdout", io.StringIO()),
                patch.object(
                    operator, "configured", side_effect=AssertionError("no config")
                ),
            ):
                with self.assertRaises(SystemExit) as stopped:
                    operator.main(args)
                self.assertEqual(stopped.exception.code, 0)
        with patch("sys.stderr", io.StringIO()):
            with self.assertRaises(SystemExit) as stopped:
                operator.main(["worker"])
            self.assertEqual(stopped.exception.code, 2)

    def test_status_filters_exact_key_and_sha(self):
        import test_object_restore as fixture

        f = fixture.ObjectRestore()
        f.setUp()
        self.addCleanup(f.doCleanups)
        with (
            patch.object(core, "load", return_value=f.config),
            patch(
                "sys.argv",
                ["open_assets", "status", "all", "--key", "assets", "--sha", f.sha],
            ),
            patch("sys.stdout", io.StringIO()) as output,
        ):
            core.main()
        lines = output.getvalue().splitlines()
        self.assertEqual(len(lines), 2)
        self.assertTrue(all(line.split()[2] == f.sha for line in lines))

    def test_runtime_failures_are_redacted_and_helpful(self):
        with (
            patch.object(core, "load", side_effect=OSError("secret URL")),
            patch("sys.argv", ["open_assets", "status"]),
            patch("sys.stderr", io.StringIO()) as output,
        ):
            with self.assertRaises(SystemExit) as stopped:
                core.main()
        self.assertEqual(stopped.exception.code, 1)
        result = json.loads(output.getvalue())
        self.assertEqual(result["status"], "failed")
        self.assertIn("doctor", result["hint"])
        self.assertNotIn("secret URL", output.getvalue())
