"""Initialize the fixed content/images and storage identities on a fresh stack.

This script writes users and named policies; it is not a read-only check or an
idempotent substitute for repository onboarding. Use repo ensure after setup.
"""

import shutil
import pathlib
import json
import subprocess
import urllib.request
import base64
import yaml
import os
from bootstrap_guard import initialize, complete, SetupRefused

p = pathlib.Path("/app/open-assets/private")
data = pathlib.Path("/data/asset-stack")
try:
    initialization = initialize(p)
except SetupRefused as error:
    raise SystemExit("Initialization refused: " + str(error)) from None


def sec(s):
    return (p / s).read_text().strip()


def cmd(args):
    r = subprocess.run(args, text=True, capture_output=True)
    if r.returncode:
        raise RuntimeError("command failed: " + r.stderr[-500:])
    return r.stdout


# Create local administrator without revealing generated password.
r = subprocess.run(
    [
        "docker",
        "exec",
        "-u",
        "20001:20001",
        "asset-stack-gitea-1",
        "gitea",
        "admin",
        "user",
        "create",
        "--username",
        "content",
        "--password",
        sec("admin_password"),
        "--email",
        "admin@localhost.invalid",
        "--admin",
        "--must-change-password=false",
    ],
    capture_output=True,
)
if r.returncode and b"already exists" not in r.stderr + r.stdout:
    raise RuntimeError("Gitea admin creation failed")
auth = (
    "Basic " + base64.b64encode(("content:" + sec("admin_password")).encode()).decode()
)


def api(path, value=None):
    req = urllib.request.Request(
        "http://127.0.0.1:13000/api/v1/" + path,
        data=json.dumps(value).encode() if value is not None else None,
        headers={"Authorization": auth, "Content-Type": "application/json"},
    )
    return json.load(urllib.request.urlopen(req))


try:
    r = api("repos/content/images")
except urllib.error.HTTPError as e:
    if e.code != 404:
        raise
    r = api(
        "user/repos",
        {
            "name": "images",
            "private": True,
            "auto_init": False,
            "default_branch": "main",
        },
    )
keys = api("repos/content/images/keys")
if not keys:
    api(
        "repos/content/images/keys",
        {
            "title": "open-assets-publisher",
            "key": sec("publisher/deploy_key.pub"),
            "read_only": True,
        },
    )
hooks = api("repos/content/images/hooks")
if not hooks:
    api(
        "repos/content/images/hooks",
        {
            "type": "gitea",
            "active": True,
            "events": ["push"],
            "config": {
                "url": "http://webhook:9000/hooks/publish-assets",
                "content_type": "json",
                "secret": sec("webhook_secret"),
            },
        },
    )
c = yaml.safe_load((p / "repositories.yaml").read_text())
c["repositories"][0]["repository_id"] = r["id"]
c["repositories"][0]["source_directory"] = "public"
(p / "repositories.yaml").write_text(yaml.safe_dump(c))
# Trust host keys read from the actual Gitea volume, not unauthenticated discovery.
keys = []
for f in (data / "gitea/data/ssh").glob("*.pub"):
    keys.append("[gitea]:2222 " + f.read_text().strip())
(p / "publisher/known_hosts").write_text("\n".join(keys) + "\n")
os.chown(p / "publisher/known_hosts", 20010, 20010)
# OC uses system + explicitly mounted private CA trust.
cli = p / "oc-cli"
(cli / "certs/CAs").mkdir(parents=True, exist_ok=True)

shutil.copy(p / "tls/ca.crt", cli / "certs/CAs/ca.crt")
image = "ghcr.io/soulteary/oc:RELEASE.2026-10-28T07-34-44Z@sha256:23edead1134c7f2190c3f8d29b20f301972461bc1814520ef90ef1edd5cdb405"
base = [
    "docker",
    "run",
    "--rm",
    "--network",
    "asset-stack_admin",
    "-v",
    str(cli) + ":/config",
    "-v",
    str(p / "tls/ca.crt") + ":/ca.crt:ro",
    image,
    "--config-dir",
    "/config",
]


def oc(*a):
    return cmd(base + list(a))


oc(
    "alias",
    "set",
    "store",
    "https://storage.internal:9000",
    sec("storage/root_user"),
    sec("storage/root_password"),
    "--admin-url",
    "https://storage.internal:9001",
    "--admin-ca",
    "/ca.crt",
    "--api",
    "s3v4",
    "--path",
    "on",
)
oc("mb", "--ignore-existing", "store/attachments")
pol = {
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Principal": "*",
            "Action": ["s3:GetObject"],
            "Resource": ["arn:aws:s3:::attachments/repositories/*"],
        }
    ],
}
(cli / "public.json").write_text(json.dumps(pol))
oc("policy", "set-json", "/config/public.json", "store/attachments")
for user, secret in [
    (sec("publisher/publisher_access"), sec("publisher/publisher_secret")),
    (sec("console_user"), sec("console_password")),
]:
    oc("admin", "user", "add", "store", user, secret)
policy = {
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Action": [
                "s3:ListBucket",
                "s3:GetBucketLocation",
                "s3:ListBucketMultipartUploads",
            ],
            "Resource": ["arn:aws:s3:::attachments"],
        },
        {
            "Effect": "Allow",
            "Action": [
                "s3:GetObject",
                "s3:PutObject",
                "s3:AbortMultipartUpload",
                "s3:ListMultipartUploadParts",
            ],
            "Resource": ["arn:aws:s3:::attachments/repositories/main-images/*"],
        },
    ],
}
(cli / "publisher.json").write_text(json.dumps(policy))
oc("admin", "policy", "add", "store", "assets-publish", "/config/publisher.json")
oc(
    "admin",
    "policy",
    "set",
    "store",
    "assets-publish",
    "user=" + sec("publisher/publisher_access"),
)
oc("admin", "policy", "set", "store", "consoleAdmin", "user=" + sec("console_user"))
complete(initialization)
print("Repository, signed hook, trusted host keys and storage identities initialized")
