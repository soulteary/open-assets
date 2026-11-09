"""Create configuration for a new single-node deployment; refuse existing state."""

import json
import os
import pathlib as pl
import secrets
import shutil
import subprocess

import bcrypt
import yaml
from deployment_config import DEFAULT_NODE_NAME, build_domains, render_env
from internal_certs import generate
from certs_maker import ensure_binary
from bootstrap_guard import prepare, complete, unused, SetupRefused

P = pl.Path
app = P("/app/open-assets")
p = app / "private"
d = P("/data/asset-stack")
os.umask(0o077)
try:
    unused(app, d)
except SetupRefused as error:
    raise SystemExit("Preparation refused: " + str(error)) from None
base = os.environ.get("PUBLIC_BASE")
if not base:
    raise SystemExit("PUBLIC_BASE is required; see docs/deployment.md")
node_name = os.environ.get("NODE_NAME", DEFAULT_NODE_NAME)
dom = build_domains(node_name, base)
try:
    preparation = prepare(app, d)
except SetupRefused as error:
    raise SystemExit("Preparation refused: " + str(error)) from None
for s in ["storage", "publisher", "tls/storage", "tls/console", "generated"]:
    (p / s).mkdir(parents=True, exist_ok=True)
for s, uid in [
    ("publisher", 20010),
    ("publisher/manifests", 20010),
    ("publisher/object-retention", 20010),
    ("images", 20010),
    ("info-refresh", 20010),
    ("otterio", 20002),
    ("gitea/data", 20001),
    ("gitea/config", 20001),
    ("console", 20004),
    ("archives", 20004),
]:
    t = d / s
    t.mkdir(parents=True, exist_ok=True)
    os.chown(t, uid, uid)
    t.chmod(0o750)


guard = d / "publisher/object-retention/compaction.lock"
with guard.open("xb"):
    pass
os.chown(guard, 20010, 20010)
guard.chmod(0o644)


def sec(s, value=None):
    t = p / s
    if not t.exists():
        t.write_text(value or secrets.token_hex(24))
        t.chmod(0o600)
    return t.read_text().strip()


for s, v in [
    ("storage/root_user", "assets-root"),
    ("storage/root_password", None),
    ("publisher/publisher_access", "assets-publisher"),
    ("publisher/publisher_secret", None),
    ("webhook_secret", None),
    ("admin_password", None),
    ("stargate_password", None),
    ("console_user", "assets-operator"),
    ("console_password", None),
]:
    sec(s, v)
shutil.copy(p / "webhook_secret", p / "publisher/webhook_secret")


def run(*a):
    subprocess.run(a, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if not (p / "publisher/deploy_key").exists():
    run(
        "ssh-keygen",
        "-q",
        "-t",
        "ed25519",
        "-N",
        "",
        "-f",
        str(p / "publisher/deploy_key"),
    )
tls = p / "tls"
if not (tls / "ca.crt").exists():
    # The directory skeleton above is empty; Certs Maker owns the new TLS tree.
    for empty in [tls / "storage", tls / "console"]:
        empty.rmdir()
    template = dict(
        line.split("=", 1)
        for line in (app / ".env.example").read_text().splitlines()
        if "=" in line and not line.startswith("#")
    )
    generate(tls, dom, ensure_binary(app, template))
shutil.copy(tls / "ca.crt", p / "publisher/ca.crt")
env = render_env((app / ".env.example").read_text(), node_name, base)
(app / ".env").write_text(env)

h = bcrypt.hashpw(sec("stargate_password").encode(), bcrypt.gensalt()).decode()
(p / "stargate.env").write_text(
    "\n".join(
        [
            "AUTH_HOST=" + dom["auth"],
            "PORT=8080",
            "PASSWORDS=" + repr("bcrypt:" + h),
            "COOKIE_SECURE=true",
            "SESSION_EXCHANGE_REDIRECT_PATH=/__auth_return",
            "CALLBACK_ALLOWED_HOSTS="
            + ",".join(dom[k] for k in ["storage", "traefik", "git", "info"]),
            "SESSION_EXCHANGE_SECRET=" + sec("session_secret"),
            "TRUSTED_PROXIES=172.29.19.2",
            "WARDEN_ENABLED=false",
            "HERALD_ENABLED=false",
            "SESSION_STORAGE_ENABLED=false",
        ]
    )
    + "\n"
)
c = yaml.safe_load((app / "config/repositories.example.yaml").read_text())
c["endpoint"] = "https://storage.internal:9000"
(p / "repositories.yaml").write_text(yaml.safe_dump(c))
(p / "domains.json").write_text(json.dumps(dom))
p.chmod(0o711)
for t in [p / "publisher", p / "generated"]:
    t.chmod(0o750)
    os.chown(t, 20010, 20010)
for t in (p / "publisher").iterdir():
    os.chown(t, 20010, 20010)
for t in [p / "webhook_secret", p / "repositories.yaml"]:
    os.chown(t, 20010, 20010)
    t.chmod(0o640)
for t in [p / "storage", tls / "storage"]:
    t.chmod(0o750)
    os.chown(t, 20002, 20002)
    for f in t.iterdir():
        os.chown(f, 20002, 20002)
        f.chmod(0o750 if f.is_dir() else 0o640)
tls.chmod(0o711)
(tls / "console").chmod(0o700)
os.chown(tls / "console", 20004, 20004)
for f in (tls / "console").iterdir():
    os.chown(f, 20004, 20004)
complete(preparation)
print(
    "Private credentials, certificates and configuration prepared; existing Traefik unchanged"
)
