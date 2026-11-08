import importlib
import json
import os
import pathlib
import tempfile
import unittest

m = importlib.import_module("open_assets.__main__")


class Core(unittest.TestCase):
    def setUp(self):
        self.t = tempfile.TemporaryDirectory()
        self.p = pathlib.Path(self.t.name)
        self.c = {
            "state_root": str(self.p / "state"),
            "revision": "test",
            "repositories": [{"key": "images", "enabled": True}],
        }

    def tearDown(self):
        self.t.cleanup()

    def test_enqueue_deduplicates(self):
        a = m.enqueue(self.c, "images", "a" * 40, "one")
        b = m.enqueue(self.c, "images", "a" * 40, "two")
        self.assertEqual(a, b)
        self.assertEqual(len(list((self.p / "state/jobs").glob("*"))), 1)

    def test_bad_sha(self):
        for sha in ["../etc", "0" * 40, "a" * 39]:
            with self.assertRaises(ValueError):
                m.enqueue(self.c, "images", sha, "x")

    def test_disabled(self):
        self.c["repositories"][0]["enabled"] = False
        with self.assertRaises(ValueError):
            m.enqueue(self.c, "images", "a" * 40, "x")

    def test_switch_and_rollback(self):
        (self.p / "releases/a").mkdir(parents=True)
        (self.p / "releases/b").mkdir()
        m.switch(self.p, "a")
        self.assertEqual(os.readlink(self.p / "current"), "releases/a")
        m.switch(self.p, "b")
        self.assertEqual(os.readlink(self.p / "current"), "releases/b")

    def test_job_history_is_distinct_from_current_image_reference(self):
        self.c["images_root"] = str(self.p / "images")
        base = self.p / "images/repos/images"
        sha = "a" * 40
        (base / "releases" / ("images-" + sha)).mkdir(parents=True)
        m.switch(base, "images-" + sha)
        m.atomic(self.p / "state/published/images", dict(sha=sha))
        self.assertEqual(m.active_image_state(self.c, "images", sha), "current")
        self.assertEqual(
            m.active_image_state(self.c, "images", "b" * 40), "other-version"
        )
        m.atomic(self.p / "state/published/images", dict(sha="b" * 40))
        self.assertEqual(m.active_image_state(self.c, "images", sha), "unknown")

    def test_atomic_complete_json(self):
        p = self.p / "job"
        m.atomic(p, {"x": 1})
        m.atomic(p, {"x": 2})
        self.assertEqual(json.loads(p.read_text()), {"x": 2})


class GitExport(unittest.TestCase):
    def test_svg_is_attachment_only_not_an_image_publication(self):
        import subprocess

        with tempfile.TemporaryDirectory() as t:
            root = pathlib.Path(t)

            def git(*args):
                return subprocess.check_output(
                    ["git", "-C", str(root), *args], stderr=subprocess.DEVNULL
                )

            git("init")
            git("config", "commit.gpgsign", "false")
            git("config", "user.name", "Fixture")
            git("config", "user.email", "fixture@localhost.invalid")
            (root / "public/images").mkdir(parents=True)
            (root / "public/images/pic.png").write_bytes(b"fixture")
            svg = b'<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>'
            (root / "public/diagram.svg").write_bytes(svg)
            git("add", ".")
            git("commit", "-m", "SVG attachment")
            config = dict(repositories=[dict(mount_path="/")])
            repository = dict(source_directory="public", mount_path="/")
            files = m.export(
                config,
                repository,
                root / ".git",
                git("rev-parse", "HEAD").decode().strip(),
                root / "out",
            )
            self.assertEqual(
                next(f["content_type"] for f in files if f["path"] == "diagram.svg"),
                "image/svg+xml",
            )
            (root / "public/diagram.svg").rename(root / "public/images/diagram.SVG")
            git("add", "public")
            git("commit", "-m", "SVG image")
            with self.assertRaisesRegex(ValueError, "image format not allowed"):
                m.export(
                    config,
                    repository,
                    root / ".git",
                    git("rev-parse", "HEAD").decode().strip(),
                    root / "out2",
                )

    def test_real_git_export_and_symlink_rejection(self):
        import subprocess

        with tempfile.TemporaryDirectory() as t:
            root = pathlib.Path(t)
            r = root / "repo"
            r.mkdir()

            def git(*a):
                return subprocess.check_output(
                    ["git", "-C", str(r), *a], stderr=subprocess.DEVNULL
                )

            git("init")
            git("config", "commit.gpgsign", "false")
            git("config", "user.name", "test")
            git("config", "user.email", "test@localhost.invalid")
            (r / "public/images").mkdir(parents=True)
            (r / "public/images/a.png").write_bytes(b"PNG")
            (r / "public/document.txt").write_text("attachment")
            git("add", ".")
            git("commit", "-m", "fixture")
            sha = git("rev-parse", "HEAD").decode().strip()
            out = root / "out"
            out.mkdir()
            cfg = {"repositories": [{"mount_path": "/"}]}
            repo = {"source_directory": "public", "mount_path": "/"}
            files = m.export(cfg, repo, r / ".git", sha, out)
            self.assertEqual(
                {f["path"] for f in files}, {"images/a.png", "document.txt"}
            )
            os.symlink("/etc/passwd", r / "public/images/escape.png")
            git("add", ".")
            git("commit", "-m", "unsafe")
            sha = git("rev-parse", "HEAD").decode().strip()
            with self.assertRaises(ValueError):
                m.export(cfg, repo, r / ".git", sha, out)


class Webhook(unittest.TestCase):
    def test_signature_and_durable_enqueue(self):
        import hashlib
        import hmac
        import threading
        import urllib.error
        import urllib.request

        import yaml

        with tempfile.TemporaryDirectory() as t:
            p = pathlib.Path(t)
            secret = p / "secret"
            secret.write_text("test-secret")
            cfg = {
                "schema_version": 1,
                "state_root": str(p / "state"),
                "images_root": str(p / "images"),
                "repositories": [
                    {
                        "key": "images",
                        "repository_id": 1,
                        "enabled": True,
                        "remote": "ssh://git@gitea:2222/content/images.git",
                        "branch": "main",
                        "source_directory": "public",
                        "mount_path": "/",
                    }
                ],
            }
            cp = p / "config"
            cp.write_text(yaml.safe_dump(cfg))
            old = m.CONFIG
            m.CONFIG = str(cp)
            os.environ["WEBHOOK_SECRET_FILE"] = str(secret)
            server = m.http.server.ThreadingHTTPServer(("127.0.0.1", 0), m.Handler)
            threading.Thread(target=server.serve_forever, daemon=True).start()
            try:
                body = json.dumps(
                    {
                        "repository": {"id": 1},
                        "ref": "refs/heads/main",
                        "after": "a" * 40,
                    }
                ).encode()
                url = f"http://127.0.0.1:{server.server_port}/hooks/publish-assets"

                def request(signature):
                    return urllib.request.urlopen(
                        urllib.request.Request(
                            url,
                            data=body,
                            headers={
                                "X-Gitea-Event": "push",
                                "X-Gitea-Signature": signature,
                            },
                        )
                    )

                with self.assertRaises(urllib.error.HTTPError) as e:
                    request("bad")
                self.assertEqual(e.exception.code, 403)
                sig = hmac.new(b"test-secret", body, hashlib.sha256).hexdigest()
                with request(sig) as response:
                    self.assertEqual(response.status, 202)
                self.assertEqual(len(list((p / "state/jobs").glob("*.json"))), 1)
                cfg["resource_limits"] = dict(
                    queue_active_records=1, queue_total_records=1
                )
                cp.write_text(yaml.safe_dump(cfg))
                with request(sig) as response:
                    self.assertEqual(response.status, 202)
                body = body.replace(b"a" * 40, b"b" * 40)
                sig = hmac.new(b"test-secret", body, hashlib.sha256).hexdigest()
                with self.assertRaises(urllib.error.HTTPError) as refused:
                    request(sig)
                self.assertEqual(refused.exception.code, 503)
                self.assertEqual(refused.exception.headers["Retry-After"], "60")
                self.assertEqual(
                    json.loads(refused.exception.read())["limit"],
                    "queue_active_records",
                )
                self.assertEqual(len(list((p / "state/jobs").glob("*.json"))), 1)
            finally:
                server.shutdown()
                server.server_close()
                m.CONFIG = old


class AuthenticationNavigation(unittest.TestCase):
    def test_return_route_remains_authenticated_and_host_scoped(self):
        import subprocess
        import sys

        with tempfile.TemporaryDirectory() as t:
            p = pathlib.Path(t)
            domains = {
                k: f"{k}-node-name.console.lab.dev"
                for k in ["git", "storage", "traefik", "auth", "info"]
            }
            domains.update(img="img-node-name.lab.dev", oss="oss-node-name.lab.dev")
            (p / "domains.json").write_text(json.dumps(domains))
            (p / "ca.pem").write_text("test-ca")
            subprocess.run(
                [
                    sys.executable,
                    "scripts/configure-edge.py",
                    "--domains",
                    str(p / "domains.json"),
                    "--ca",
                    str(p / "ca.pem"),
                    "--output",
                    str(p / "routes.yaml"),
                ],
                check=True,
            )
            routes = json.loads((p / "routes.yaml").read_text())["http"]["routers"]
            self.assertEqual(
                routes["info"]["middlewares"], ["console-auth", "console-gzip"]
            )
            landing = routes["auth-return"]
            self.assertEqual(landing["middlewares"], ["console-auth", "console-gzip"])
            self.assertIn("Path(`/__auth_return`)", landing["rule"])
            for k in ["git", "storage", "traefik", "info"]:
                self.assertIn(f"Host(`{domains[k]}`)", landing["rule"])
            for k in ["img", "oss", "auth"]:
                self.assertNotIn(domains[k], landing["rule"])
            self.assertNotIn("headers", json.dumps(routes))


if __name__ == "__main__":
    unittest.main()
