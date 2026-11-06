import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import yaml

from open_assets import admin, __main__ as core


class FakeManager(admin.Manager):
    def __init__(self, root):
        self.root = Path(root)
        self.private = self.root / "private"
        self.env = {"DATA_ROOT": str(self.root / "data")}
        self.repositories = {}
        self.keys = []
        self.hooks = []
        self.restarts = 0
        self.fail_policy_once = False
        self.fail_nginx_once = False
        self.live_policy = {
            "Version": "2012-10-17",
            "Statement": [
                {
                    "Effect": "Allow",
                    "Action": ["s3:PutObject", "s3:GetObject"],
                    "Resource": ["arn:aws:s3:::attachments/repositories/existing/*"],
                }
            ],
        }
        self.private.mkdir()
        (self.private / "publisher").mkdir()
        (self.private / "oc-cli").mkdir()
        (self.private / "generated").mkdir()
        (self.private / "publisher/deploy_key.pub").write_text(
            "ssh-ed25519 TEST-PUBLIC-KEY publisher"
        )
        (self.private / "publisher/publisher_access").write_text("test-publisher")
        (self.private / "webhook_secret").write_text("test-webhook-secret")
        config = yaml.safe_load(Path("config/repositories.example.yaml").read_text())
        (self.private / "repositories.yaml").write_text(yaml.safe_dump(config))
        core.render(config, self.private / "generated")

    def api(self, path, value=None, method=None, missing=False):
        path = path.split("?")[0]
        if path == "users/content":
            return {"type": "User"}
        if path == "user/repos":
            self.repositories["content/" + value["name"]] = {
                "id": 20,
                "private": True,
                "archived": False,
            }
            return self.repositories["content/" + value["name"]]
        if path.endswith("/keys"):
            if value is not None:
                self.keys.append(dict(id=1, **value))
            return copy.deepcopy(self.keys)
        if path.endswith("/hooks"):
            if value is not None:
                self.hooks.append(dict(id=1, **value))
            return copy.deepcopy(self.hooks)
        if "/hooks/" in path:
            self.hooks[0] = dict(id=1, **value)
            return self.hooks[0]
        return self.repositories.get(path.removeprefix("repos/"))

    def oc(self, *args):
        return json.dumps({"policyJSON": self.live_policy})

    def apply_policy(self):
        if self.fail_policy_once:
            self.fail_policy_once = False
            raise RuntimeError("injected policy failure")
        self.live_policy = json.loads(
            (self.private / "oc-cli/publisher.json").read_text()
        )

    def compose(self, *args):
        if args[0] == "up":
            self.restarts += 1
        if args[0] == "run" and "render" in args:
            core.render(
                yaml.safe_load((self.private / "repositories.yaml").read_text()),
                self.private / "generated/.onboarding",
            )
        if args[0] == "exec" and "-t" in args and self.fail_nginx_once:
            self.fail_nginx_once = False
            raise RuntimeError("injected nginx failure")
        return ""


class Onboarding(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.chown = patch("open_assets.admin.os.chown")
        self.chown.start()
        self.manager = FakeManager(self.tmp.name)

    def tearDown(self):
        self.chown.stop()
        self.tmp.cleanup()

    def ensure(self):
        self.manager.ensure("content/project-c", "project-c", "/project-c/", "main")

    def test_repeat_reuses_resources_and_preserves_policy(self):
        self.ensure()
        self.ensure()
        self.assertEqual(len(self.manager.repositories), 1)
        self.assertEqual(self.manager.restarts, 1)
        self.assertEqual(len(self.manager.keys), 1)
        self.assertEqual(len(self.manager.hooks), 1)
        config = yaml.safe_load(
            (self.manager.private / "repositories.yaml").read_text()
        )
        self.assertEqual(len(config["repositories"]), 2)
        resources = self.manager.live_policy["Statement"][0]["Resource"]
        self.assertEqual(
            resources.count("arn:aws:s3:::attachments/repositories/project-c/*"), 1
        )
        self.assertIn("arn:aws:s3:::attachments/repositories/existing/*", resources)
        self.assertTrue(self.manager.keys[0]["read_only"])

    def test_mount_conflict_precedes_creation(self):
        with self.assertRaises(admin.Conflict):
            self.manager.ensure("content/conflict", "conflict", "/", "main")
        self.assertEqual(self.manager.repositories, {})

    def test_bound_key_cannot_switch_repository(self):
        self.ensure()
        with self.assertRaises(admin.Conflict):
            self.manager.ensure("content/other", "project-c", "/project-c/", "main")
        self.assertEqual(len(self.manager.repositories), 1)

    def test_policy_failure_retry_converges(self):
        self.manager.fail_policy_once = True
        with self.assertRaises(RuntimeError):
            self.ensure()
        self.ensure()
        self.assertEqual(len(self.manager.keys), 1)
        self.assertEqual(len(self.manager.hooks), 1)

    def test_nginx_failure_restores_previous_mapping_then_retry(self):
        before = (self.manager.private / "generated/images.conf").read_text()
        self.manager.fail_nginx_once = True
        with self.assertRaises(RuntimeError):
            self.ensure()
        self.assertEqual(
            (self.manager.private / "generated/images.conf").read_text(), before
        )
        self.ensure()
        self.assertIn(
            "/project-c/", (self.manager.private / "generated/images.conf").read_text()
        )

    def test_public_repository_is_not_adopted(self):
        self.manager.repositories["content/project-c"] = {
            "id": 20,
            "private": False,
            "archived": False,
        }
        with self.assertRaises(admin.Conflict):
            self.ensure()
        self.assertEqual(self.manager.keys, [])
        self.assertEqual(self.manager.hooks, [])

    def test_existing_write_key_and_duplicate_hooks_are_rejected(self):
        self.ensure()
        self.manager.keys[0]["read_only"] = False
        with self.assertRaises(admin.Conflict):
            self.ensure()
        self.manager.keys[0]["read_only"] = True
        self.manager.hooks.append(copy.deepcopy(self.manager.hooks[0]))
        with self.assertRaises(admin.Conflict):
            self.ensure()

    def test_unsafe_names_rejected(self):
        for name in ["../etc", "content/name;touch", "content/name/extra"]:
            with self.assertRaises(admin.Conflict):
                self.manager.ensure(name, "safe", "/safe/", "main")


if __name__ == "__main__":
    unittest.main()
