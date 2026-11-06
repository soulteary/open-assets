import bcrypt
import os
import pathlib as pl
import secrets
import subprocess
import json
import yaml
import shutil

P = pl.Path
app = P("/app/open-assets")
p = app / "private"
d = P("/data/asset-stack")
os.umask(0o077)
base = os.environ["PUBLIC_BASE"]
dom = {
    k: f"{k}-toh1."
    + ("console." if k in ["storage", "git", "auth", "traefik"] else "")
    + base
    for k in ["oss", "img", "storage", "git", "auth", "traefik"]
}
for s in ["storage", "publisher", "tls/storage", "tls/console", "generated"]:
    (p / s).mkdir(parents=True, exist_ok=True)
for s, uid in [
    ("publisher", 20010),
    ("images", 20010),
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
if not (tls / "ca.key").exists():
    run(
        "openssl",
        "req",
        "-x509",
        "-newkey",
        "rsa:3072",
        "-nodes",
        "-days",
        "3650",
        "-subj",
        "/CN=Open Assets Private CA",
        "-keyout",
        str(tls / "ca.key"),
        "-out",
        str(tls / "ca.crt"),
    )
for name, cn, cert, key in [
    ("storage", "storage.internal", "public.crt", "private.key"),
    ("console", dom["storage"], "console.crt", "console.key"),
]:
    t = tls / name
    if not (t / cert).exists():
        (t / "extensions").write_text(
            f"subjectAltName=DNS:{cn},DNS:otterio\nextendedKeyUsage=serverAuth\n"
        )
        run(
            "openssl",
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=" + cn,
            "-keyout",
            str(t / key),
            "-out",
            str(t / "request.csr"),
        )
        run(
            "openssl",
            "x509",
            "-req",
            "-in",
            str(t / "request.csr"),
            "-CA",
            str(tls / "ca.crt"),
            "-CAkey",
            str(tls / "ca.key"),
            "-CAcreateserial",
            "-days",
            "365",
            "-extfile",
            str(t / "extensions"),
            "-out",
            str(t / cert),
        )
    (tls / "storage/CAs").mkdir(exist_ok=True)
    shutil.copy(tls / "ca.crt", t / "ca.crt")
shutil.copy(tls / "ca.crt", p / "publisher/ca.crt")
env = (
    (app / ".env.example")
    .read_text()
    .replace("storage-toh1.console.lab.dev", dom["storage"])
    .replace("git-toh1.console.lab.dev", dom["git"])
)
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
            + ",".join(dom[k] for k in ["storage", "traefik", "git"]),
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
print(
    "Private credentials, certificates and configuration prepared; existing Traefik unchanged"
)
