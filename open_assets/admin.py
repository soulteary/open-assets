"""Reconcile one repository using host-only administrative credentials.

Gitea, IAM and local files have separate commit boundaries. Keep recovery copies
and converge on rerun rather than deleting remote resources on failure. The
application containers do not receive this module's host privileges.
"""

import argparse
import base64
import copy
import fcntl
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request

import yaml

from . import __main__ as core


class Conflict(RuntimeError):
    pass


def validate_identity(name, key, mount, branch):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*", name):
        raise Conflict("name must be owner/repository")
    if not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", key):
        raise Conflict("invalid repository key")
    if not re.fullmatch(r"/(?:[a-z0-9][a-z0-9-]*/)?", mount):
        raise Conflict("invalid mount path")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/-]*", branch) or ".." in branch:
        raise Conflict("invalid branch")


def desired_config(config, name, key, mount, branch, repository_id):
    """Append a binding or verify its identity; preserve existing quotas and intent."""
    result = copy.deepcopy(config)
    existing = next((r for r in result["repositories"] if r["key"] == key), None)
    desired = dict(
        key=key,
        name=name,
        repository_id=repository_id,
        remote=f"ssh://git@gitea:2222/{name}.git",
        branch=branch,
        enabled=True,
        serve=True,
        source_directory="public",
        mount_path=mount,
        max_files=10000,
        max_bytes=1073741824,
        max_file_bytes=104857600,
    )
    if existing:
        for field in [
            "name",
            "repository_id",
            "remote",
            "branch",
            "source_directory",
            "mount_path",
        ]:
            if existing[field] != desired[field]:
                raise Conflict(f"existing key has different {field}")
        if not existing.get("enabled") or not existing.get("serve", True):
            raise Conflict(
                "existing repository disabled; enable explicitly before onboarding"
            )
        return result
    for r in result["repositories"]:
        if r["name"].lower() == name.lower() or r["repository_id"] == repository_id:
            raise Conflict("repository already bound to another key")
        old = r["mount_path"]
        if old == mount or (
            old != "/"
            and mount != "/"
            and (old.startswith(mount) or mount.startswith(old))
        ):
            raise Conflict("URL prefix conflicts with existing repository")
    result["repositories"].append(desired)
    return result


def extend_policy(policy, bucket, key):
    """Extend the explicit publisher object scope without rebuilding its policy."""
    result = copy.deepcopy(policy)
    statements = [
        s
        for s in result["Statement"]
        if s.get("Effect") == "Allow" and "s3:PutObject" in s.get("Action", [])
    ]
    if len(statements) != 1:
        raise Conflict("expected one explicit publisher object-write policy statement")
    statement = statements[0]
    resources = statement["Resource"]
    if isinstance(resources, str):
        resources = [resources]
    target = f"arn:aws:s3:::{bucket}/repositories/{key}/*"
    if target not in resources:
        resources.append(target)
    statement["Resource"] = resources
    return result


def write_private(path, value, uid=0):
    """Durable replacement; callers recreate single-file bind mounts when needed."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(dir=path.parent)
    try:
        with os.fdopen(fd, "w") as f:
            f.write(value)
            f.flush()
            os.fsync(f.fileno())
        os.chown(temporary, uid, uid)
        os.chmod(temporary, 0o640 if uid else 0o600)
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


class Manager:
    def __init__(self, root):
        self.root = Path(root).resolve()
        self.private = self.root / "private"
        auth = "content:" + (self.private / "admin_password").read_text().strip()
        self.auth = "Basic " + base64.b64encode(auth.encode()).decode()
        self.env = dict(
            line.split("=", 1)
            for line in (self.root / ".env").read_text().splitlines()
            if line and not line.startswith("#") and "=" in line
        )

    def api(self, path, value=None, method=None, missing=False):
        request = urllib.request.Request(
            "http://127.0.0.1:13000/api/v1/" + path,
            data=json.dumps(value).encode() if value is not None else None,
            method=method,
            headers={"Authorization": self.auth, "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                body = response.read()
                return json.loads(body) if body else None
        except urllib.error.HTTPError as error:
            if missing and error.code == 404:
                return None
            raise RuntimeError(f"Gitea request rejected (HTTP {error.code})") from None

    def command(self, *args):
        response = subprocess.run(
            args, cwd=self.root, capture_output=True, text=True, timeout=180
        )
        if response.returncode:
            # Subprocess output can contain credentials or private endpoint names.
            raise RuntimeError(
                f"{args[0]} command failed; inspect private service state"
            )
        return response.stdout

    def compose(self, *args):
        return self.command("docker", "compose", *args)

    def oc(self, *args):
        return self.command(
            "docker",
            "run",
            "--rm",
            "--network",
            "asset-stack_admin",
            "-v",
            str(self.private / "oc-cli") + ":/config",
            "-v",
            str(self.private / "tls/ca.crt") + ":/ca.crt:ro",
            self.env["OC_IMAGE"],
            "--config-dir",
            "/config",
            "--json",
            *args,
        )

    def collection(self, path):
        result = []
        page = 1
        while True:
            rows = self.api(f"{path}?limit=50&page={page}")
            result.extend(rows)
            if len(rows) < 50:
                return result
            page += 1

    def apply_policy(self):
        self.oc(
            "admin",
            "policy",
            "add",
            "store",
            "assets-publish",
            "/config/publisher.json",
        )
        user = (self.private / "publisher/publisher_access").read_text().strip()
        identity = json.loads(self.oc("admin", "user", "info", "store", user))
        if identity.get("userStatus") != "enabled":
            raise Conflict("publisher account is not enabled")
        if "assets-publish" not in identity.get("policyName", "").split(","):
            self.oc(
                "admin", "policy", "update", "store", "assets-publish", "user=" + user
            )

    def check_root_prefix(self, config, key, mount):
        if not any(r["key"] == key for r in config["repositories"]) and mount != "/":
            for r in config["repositories"]:
                if r["mount_path"] == "/":
                    root = (
                        Path(self.env["DATA_ROOT"])
                        / "images/repos"
                        / r["key"]
                        / "current"
                    )
                    if (root / mount.strip("/")).exists():
                        raise Conflict(
                            "new prefix shadows files in the root image repository"
                        )

    def ensure(self, name, key, mount, branch):
        """Reconcile remote resources, then apply local config and URL mappings.

        ready means onboarding completed, not that content has been published.
        Hooks may exist before the local binding is ready; callers must wait
        before pushing and redeliver events rejected during maintenance.
        """
        self.stage = "preflight"
        validate_identity(name, key, mount, branch)
        config_path = self.private / "repositories.yaml"
        original = config_path.read_text()
        config = yaml.safe_load(original)
        # Reject naming/prefix conflicts before creating any external resources.
        candidate_id = next(
            (r["repository_id"] for r in config["repositories"] if r["key"] == key),
            max([r["repository_id"] for r in config["repositories"]] + [0]) + 1,
        )
        candidate = desired_config(config, name, key, mount, branch, candidate_id)
        with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml") as f:
            yaml.safe_dump(candidate, f)
            f.flush()
            previous_config = core.CONFIG
            try:
                core.CONFIG = f.name
                core.load()
            finally:
                core.CONFIG = previous_config
        self.check_root_prefix(config, key, mount)
        policy_path = self.private / "oc-cli/publisher.json"
        # The server policy is authoritative; a stale local copy may omit
        # permissions added since the previous onboarding.
        live_policy = json.loads(
            self.oc("admin", "policy", "info", "store", "assets-publish")
        )
        policy_original = json.dumps(live_policy["policyJSON"], indent=2)
        policy = extend_policy(json.loads(policy_original), config["bucket"], key)
        # Check prerequisites before mutations.
        public_key = (self.private / "publisher/deploy_key.pub").read_text().strip()
        hook_secret = (self.private / "webhook_secret").read_text().strip()
        if len(public_key.split()) < 2 or not hook_secret:
            raise Conflict("missing publisher key or webhook secret")
        self.compose("config", "--quiet")
        self.stage = "gitea-repository"
        repository = self.api("repos/" + name, missing=True)
        owner, short = name.split("/")
        if repository is None:
            endpoint = "user/repos"
            if owner != "content":
                if self.api("orgs/" + owner, missing=True) is None:
                    raise Conflict(
                        "creation supports content or an accessible organization"
                    )
                endpoint = "orgs/" + owner + "/repos"
            repository = self.api(
                endpoint,
                dict(name=short, private=True, auto_init=False, default_branch=branch),
            )
        if not repository.get("private") or repository.get("archived"):
            raise Conflict("existing repository must be private and active")
        desired = desired_config(config, name, key, mount, branch, repository["id"])
        self.stage = "deploy-key"
        keys = self.collection("repos/" + name + "/keys")
        matching = [k for k in keys if k["key"].split()[:2] == public_key.split()[:2]]
        if matching and not matching[0].get("read_only"):
            raise Conflict("publisher key has write access; refusing to adopt it")
        if not matching:
            self.api(
                "repos/" + name + "/keys",
                dict(title="open-assets-publisher", key=public_key, read_only=True),
            )
        self.stage = "webhook"
        hooks = self.collection("repos/" + name + "/hooks")
        url = "http://webhook:9000/hooks/publish-assets"
        matching_hooks = [h for h in hooks if h.get("config", {}).get("url") == url]
        if len(matching_hooks) > 1:
            raise Conflict("duplicate publisher webhooks; remove duplicates explicitly")
        hook = dict(
            type="gitea",
            active=True,
            events=["push"],
            config=dict(url=url, content_type="json", secret=hook_secret),
        )
        # Secret is redacted by Gitea reads; reapply the same value without adding hooks.
        if matching_hooks:
            if matching_hooks[0].get("type") != "gitea":
                raise Conflict("publisher webhook has incompatible provider")
            self.api(f"repos/{name}/hooks/{matching_hooks[0]['id']}", hook, "PATCH")
        else:
            self.api("repos/" + name + "/hooks", hook)
        changed = desired != config
        backup = self.private / "onboarding" / key
        backup.mkdir(parents=True, exist_ok=True, mode=0o700)
        # Never overwrite the first recovery copy on retry.
        for path, text in [
            (backup / "repositories.before.yaml", original),
            (backup / "publisher.before.json", policy_original),
        ]:
            if not path.exists():
                write_private(path, text)
        # Complete desired state on retries even if the preceding run stopped mid-apply.
        write_private(policy_path, json.dumps(policy, indent=2))
        self.stage = "s3-policy"
        self.apply_policy()
        marker = backup / "result.json"
        previous_result = json.loads(marker.read_text()) if marker.exists() else {}
        # Editors may replace a bind-mounted file even if its content is equal.
        # The receipt lets a routine repeat avoid restarting services while an
        # interrupted/replaced configuration still forces a fresh mount.
        recreate = (
            changed or previous_result.get("config_inode") != config_path.stat().st_ino
        )
        self.stage = "configuration-and-nginx"
        stopped = False
        try:
            if changed:
                self.compose("stop", "webhook", "publisher")
                stopped = True
                self.check_root_prefix(config, key, mount)
                write_private(config_path, yaml.safe_dump(desired), 20010)
            self.compose("run", "--rm", "--no-deps", "publisher", "validate")
            self.compose(
                "run",
                "--rm",
                "--no-deps",
                "-v",
                str(self.private / "generated") + ":/generated",
                "publisher",
                "render",
                "/generated/.onboarding",
            )
            live = self.private / "generated/images.conf"
            old_nginx = live.read_text()
            new_nginx = (self.private / "generated/.onboarding/images.conf").read_text()
            if old_nginx != new_nginx:
                if not (backup / "images.before.conf").exists():
                    write_private(backup / "images.before.conf", old_nginx)
                # Preserve inode for the directory mount and existing permissions.
                live.write_text(new_nginx)
                try:
                    self.compose(
                        "exec",
                        "-T",
                        "nginx",
                        "nginx",
                        "-c",
                        "/etc/nginx/open-assets.conf",
                        "-t",
                    )
                    self.compose(
                        "exec",
                        "-T",
                        "nginx",
                        "nginx",
                        "-c",
                        "/etc/nginx/open-assets.conf",
                        "-s",
                        "reload",
                    )
                except Exception:
                    live.write_text(old_nginx)
                    raise
            # Recreate on configuration changes or interrupted application, not routine repeats.
            if recreate:
                self.compose(
                    "up", "-d", "--no-build", "--force-recreate", "webhook", "publisher"
                )
            stopped = False
        finally:
            if stopped:
                self.compose(
                    "up", "-d", "--no-build", "--force-recreate", "webhook", "publisher"
                )
        receipt = dict(
            name=name,
            key=key,
            repository_id=repository["id"],
            mount_path=mount,
            branch=branch,
            config_inode=config_path.stat().st_ino,
            services_recreated=recreate,
        )
        write_private(backup / "result.json", json.dumps(receipt, indent=2))
        print(
            json.dumps(
                dict(
                    status="ready",
                    **{k: v for k, v in receipt.items() if k != "config_inode"},
                ),
                ensure_ascii=False,
            )
        )


def main(argv=None):
    parser = argparse.ArgumentParser(description="Host-only Open Assets administration")
    parser.add_argument("--root", default="/app/open-assets")
    commands = parser.add_subparsers(dest="command", required=True)
    repos = commands.add_parser("repo").add_subparsers(dest="operation", required=True)
    ensure = repos.add_parser("ensure")
    ensure.add_argument("name", help="Gitea owner/repository")
    ensure.add_argument("--key", required=True)
    ensure.add_argument("--mount-path", required=True)
    ensure.add_argument("--branch", default="main")
    objects = commands.add_parser("objects").add_subparsers(
        dest="operation", required=True
    )
    object_setup = objects.add_parser(
        "setup", help="reconcile separate S3 retention credentials and scope"
    )
    object_setup.add_argument(
        "--compaction",
        action="store_true",
        help="temporarily grant scoped deletion for verified compaction; run setup again to revoke",
    )
    args = parser.parse_args(argv)
    if os.geteuid() != 0:
        parser.error(
            "run host administration with sudo; application containers remain unprivileged"
        )
    manager = Manager(args.root)
    with (manager.private / "onboarding.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            if args.command == "objects":
                from .object_retention_setup import setup

                setup(manager, compaction=args.compaction)
            else:
                manager.ensure(args.name, args.key, args.mount_path, args.branch)
        except (Conflict, RuntimeError, OSError, KeyError, ValueError) as error:
            # Deliberately omit raw remote responses and subprocess arguments.
            print(
                f"onboarding failed at {getattr(manager, 'stage', 'preflight')}: {error}",
                file=sys.stderr,
            )
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
