"""Destructive-fixture acceptance for an explicitly disposable, initialized stack.

Never runs against normal deployment domains. All generated keys and content stay
inside private/e2e. Run fresh-stack.sh only on an empty dedicated Docker daemon.
"""

import base64
import http.cookiejar
import json
import os
from pathlib import Path
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from open_assets.admin import Manager  # noqa: E402


def command(*args, env=None):
    result = subprocess.run(args, cwd=ROOT, env=env, capture_output=True, timeout=240)
    if result.returncode:
        # Captured diagnostics stay in the disposable environment, not CI stdout.
        (ROOT / "private/e2e/last-error.txt").write_bytes(result.stderr)
        raise RuntimeError("isolated command failed: " + args[0])
    return result.stdout


def wait_job(key, sha):
    for _ in range(90):
        jobs = [
            json.loads(p.read_text())
            for p in Path("/data/asset-stack/publisher/jobs").glob("*.json")
        ]
        job = next((j for j in jobs if j["key"] == key and j["sha"] == sha), None)
        if job and job["status"] in ["published", "failed", "superseded"]:
            assert job["status"] == "published", job.get("error", job["status"])
            return
        time.sleep(1)
    raise RuntimeError("publication did not complete")


def test_edge(manager, domains):
    """Private-CA gateway fixture; deliberately does not contact public ACME."""
    edge = ROOT / "private/e2e/edge"
    (edge / "dynamic").mkdir(parents=True, exist_ok=True)
    edge.chmod(0o755)
    (edge / "dynamic").chmod(0o755)
    (edge / "extensions").write_text(
        "subjectAltName="
        + ",".join("DNS:" + h for h in domains.values())
        + "\nextendedKeyUsage=serverAuth\n"
    )
    command(
        "openssl",
        "req",
        "-new",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-subj",
        "/CN=Open Assets E2E",
        "-keyout",
        str(edge / "edge.key"),
        "-out",
        str(edge / "edge.csr"),
    )
    command(
        "openssl",
        "x509",
        "-req",
        "-in",
        str(edge / "edge.csr"),
        "-CA",
        str(ROOT / "private/tls/ca.crt"),
        "-CAkey",
        str(ROOT / "private/tls/issuers/storage/root.key"),
        "-CAcreateserial",
        "-days",
        "2",
        "-extfile",
        str(edge / "extensions"),
        "-out",
        str(edge / "edge.crt"),
    )
    os.chown(edge / "edge.key", 20003, 20003)
    (edge / "edge.key").chmod(0o600)
    (edge / "edge.crt").chmod(0o644)
    command(
        "python3",
        "scripts/configure-edge.py",
        "--domains",
        "private/domains.json",
        "--ca",
        "private/tls/ca.crt",
        "--output",
        str(edge / "dynamic/routes.yaml"),
    )
    routes = json.loads((edge / "dynamic/routes.yaml").read_text())
    for router in routes["http"]["routers"].values():
        router["tls"] = {}  # Fixture uses local file certificates instead of ACME.
    (edge / "dynamic/routes.yaml").write_text(json.dumps(routes))
    (edge / "dynamic/routes.yaml").chmod(0o644)
    (edge / "dynamic/tls.yaml").write_text(
        yaml.safe_dump(
            {
                "tls": {
                    "certificates": [
                        {
                            "certFile": "/etc/traefik/edge.crt",
                            "keyFile": "/etc/traefik/edge.key",
                        }
                    ]
                }
            }
        )
    )
    (edge / "dynamic/tls.yaml").chmod(0o644)
    (edge / "static.yaml").write_text(
        yaml.safe_dump(
            {
                "entryPoints": {"websecure": {"address": ":8443"}},
                "providers": {"file": {"directory": "/etc/traefik/dynamic"}},
                "api": {"dashboard": True},
                "global": {"checkNewVersion": False, "sendAnonymousUsage": False},
                "log": {"level": "ERROR"},
            }
        )
    )
    (edge / "static.yaml").chmod(0o644)
    image = manager.env["TRAEFIK_IMAGE"]
    command("docker", "pull", image)
    digest = json.loads(command("docker", "image", "inspect", image))[0]["RepoDigests"][
        0
    ]
    compose = {
        "name": "open-assets-e2e-edge",
        "services": {
            "gateway": {
                "image": digest,
                "user": "20003:20003",
                "read_only": True,
                "cap_drop": ["ALL"],
                "command": ["--configFile=/etc/traefik/static.yaml"],
                "ports": ["127.0.0.1:443:8443"],
                "volumes": [str(edge) + ":/etc/traefik:ro"],
                "networks": {"edge": {}, "auth": {"ipv4_address": "172.29.19.2"}},
            }
        },
        "networks": {
            "edge": {"external": True, "name": "open-assets-edge"},
            "auth": {"external": True, "name": "open-assets-auth"},
        },
    }
    (edge / "compose.yaml").write_text(yaml.safe_dump(compose))
    command("docker", "compose", "-f", str(edge / "compose.yaml"), "up", "-d")
    with Path("/etc/hosts").open("a") as hosts:
        hosts.write("\n127.0.0.1 " + " ".join(domains.values()) + "\n")
    context = ssl.create_default_context(cafile=str(ROOT / "private/tls/ca.crt"))
    for _ in range(30):
        try:
            with urllib.request.urlopen(
                "https://" + domains["auth"] + "/healthz", context=context, timeout=2
            ) as response:
                assert response.status == 200
            return context
        except Exception:
            time.sleep(1)
    raise RuntimeError("test gateway not ready")


def main():
    if os.environ.get("OPEN_ASSETS_FRESH_TEST") != "1" or os.geteuid() != 0:
        raise SystemExit("requires explicit disposable-environment opt-in and root")
    os.umask(0o077)
    domains = json.loads((ROOT / "private/domains.json").read_text())
    if not all(h.endswith(".acceptance.invalid") for h in domains.values()):
        raise SystemExit("refusing non-test domains")
    work = ROOT / "private/e2e"
    work.mkdir(exist_ok=True)
    manager = Manager(ROOT)
    manager.ensure("content/e2e-images", "e2e-images", "/e2e-images/", "main")
    repository = manager.api("repos/content/e2e-images")
    counts = (
        len(manager.collection("repos/content/e2e-images/keys")),
        len(manager.collection("repos/content/e2e-images/hooks")),
    )
    starts = command(
        "docker",
        "inspect",
        "--format",
        "{{.State.StartedAt}}",
        "asset-stack-publisher-1",
    )
    manager.ensure("content/e2e-images", "e2e-images", "/e2e-images/", "main")
    assert manager.api("repos/content/e2e-images")["id"] == repository["id"]
    assert (
        counts
        == (1, 1)
        == (
            len(manager.collection("repos/content/e2e-images/keys")),
            len(manager.collection("repos/content/e2e-images/hooks")),
        )
    )
    assert starts == command(
        "docker",
        "inspect",
        "--format",
        "{{.State.StartedAt}}",
        "asset-stack-publisher-1",
    )
    print("PASS repeated onboarding: stable ID, one Key/Hook, no Worker restart")
    from open_assets.admin import Conflict

    try:
        manager.ensure("content/e2e-conflict", "e2e-conflict", "/", "main")
    except Conflict:
        pass
    else:
        raise AssertionError("prefix conflict accepted")
    assert manager.api("repos/content/e2e-conflict", missing=True) is None
    print("PASS prefix conflict: no repository created")
    key = work / "personal-key"
    command("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key))
    manager.api(
        "user/keys",
        {"title": "fresh-e2e-user", "key": key.with_suffix(".pub").read_text()},
    )
    known = work / "known_hosts"
    known.write_text(
        (ROOT / "private/publisher/known_hosts")
        .read_text()
        .replace("[gitea]:2222", "[127.0.0.1]:2222")
    )
    env = os.environ.copy()
    env["GIT_SSH_COMMAND"] = (
        f"ssh -i {key} -o UserKnownHostsFile={known} -o StrictHostKeyChecking=yes -o BatchMode=yes"
    )
    png = base64.b64decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aX1sAAAAASUVORK5CYII="
    )
    releases = []
    for name, key_name, mount in [
        ("images", "main-images", ""),
        ("e2e-images", "e2e-images", "e2e-images/"),
    ]:
        checkout = work / name
        command(
            "git",
            "clone",
            f"ssh://git@127.0.0.1:2222/content/{name}.git",
            str(checkout),
            env=env,
        )
        command("git", "-C", str(checkout), "config", "user.name", "Open Assets E2E")
        command(
            "git", "-C", str(checkout), "config", "user.email", "e2e@localhost.invalid"
        )
        command("git", "-C", str(checkout), "config", "commit.gpgsign", "false")
        if (
            command("git", "-C", str(checkout), "branch", "--show-current").strip()
            != b"main"
        ):
            command("git", "-C", str(checkout), "switch", "-c", "main")
        (checkout / "public/images").mkdir(parents=True)
        (checkout / "public/documents").mkdir()
        (checkout / "public/images/图片 +%.png").write_bytes(png)
        body = ("Fresh deployment: " + key_name + "\n").encode()
        (checkout / "public/documents/manual.txt").write_bytes(body)
        command("git", "-C", str(checkout), "add", "public")
        command(
            "git", "-C", str(checkout), "commit", "-m", "Publish isolated test assets"
        )
        command("git", "-C", str(checkout), "push", "origin", "main", env=env)
        sha = command("git", "-C", str(checkout), "rev-parse", "HEAD").decode().strip()
        wait_job(key_name, sha)
        releases.append((key_name, mount, sha, body))
        print("PASS SSH Push and published state:", key_name)
    context = test_edge(manager, domains)
    for key_name, mount, sha, body in releases:
        image_url = (
            "https://"
            + domains["img"]
            + "/"
            + urllib.parse.quote(mount + "图片 +%.png", safe="/")
        )
        object_url = (
            "https://"
            + domains["oss"]
            + "/attachments/repositories/"
            + key_name
            + "/releases/"
            + sha
            + "/documents/manual.txt"
        )
        assert urllib.request.urlopen(image_url, context=context).read() == png
        assert urllib.request.urlopen(object_url, context=context).read() == body
        request = urllib.request.Request(image_url, headers={"Range": "bytes=0-7"})
        with urllib.request.urlopen(request, context=context) as response:
            assert response.status == 206 and response.read() == png[:8]
        print("PASS verified HTTPS image, attachment and Range:", key_name)
    jar = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(
        urllib.request.HTTPCookieProcessor(jar),
        urllib.request.HTTPSHandler(context=context),
    )
    response = opener.open("https://" + domains["traefik"] + "/api/rawdata")
    assert urllib.parse.urlparse(response.url).hostname == domains["auth"]
    response.read()
    body = urllib.parse.urlencode(
        {
            "password": (ROOT / "private/stargate_password").read_text().strip(),
            "callback": domains["traefik"],
        }
    ).encode()
    response = opener.open("https://" + domains["auth"] + "/_login", body)
    assert urllib.parse.urlparse(response.url).path == "/__auth_return"
    response.read()
    assert "routers" in json.load(
        opener.open("https://" + domains["traefik"] + "/api/rawdata")
    )
    print("PASS anonymous management redirect and authenticated gateway API")
    report = {
        "result": "passed",
        "platform": os.uname().machine,
        "docker": command("docker", "version", "--format", "{{.Server.Version}}")
        .decode()
        .strip(),
        "repositories": len(releases),
        "checks": [
            "fresh initialization",
            "repeat onboarding",
            "prefix conflict",
            "SSH Push",
            "signed webhook",
            "published state",
            "HTTPS object bytes",
            "encoded image path",
            "Range",
            "management authentication",
        ],
        "certificate_scope": "test CA; no public DNS/ACME",
    }
    (work / "result.json").write_text(json.dumps(report, indent=2))
    print(json.dumps(report))


if __name__ == "__main__":
    main()
