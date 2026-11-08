"""Validate onboarding recipes only in the disposable fresh-stack fixture."""

import hashlib
import http.cookiejar
import json
import os
from pathlib import Path
import re
import shutil
import ssl
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request

import yaml

ROOT = Path(__file__).resolve().parents[2]


def command(*args):
    result = subprocess.run(args, cwd=ROOT, capture_output=True, timeout=240)
    if result.returncode:
        (ROOT / "private/e2e/community-error.txt").write_bytes(result.stderr)
        raise RuntimeError("isolated command failed: " + args[0])
    return result.stdout


def snapshot():
    state = Path("/data/asset-stack/publisher")
    paths = [
        state / name
        for name in ("jobs", "manifests", "published", "rollback-protection")
    ]
    paths.append(Path("/data/asset-stack/images"))
    return {
        str(p): ("symlink", os.readlink(p))
        if p.is_symlink()
        else ("file", hashlib.sha256(p.read_bytes()).hexdigest())
        for base in paths
        for p in base.rglob("*")
        if p.is_symlink() or p.is_file()
    }


def main():
    if os.environ.get("OPEN_ASSETS_COMMUNITY_TEST") != "1" or os.geteuid() != 0:
        raise SystemExit("requires explicit disposable-environment opt-in and root")
    if ROOT != Path("/app/open-assets"):
        raise SystemExit("requires disposable /app/open-assets fixture")
    os.chdir(ROOT)
    os.umask(0o077)
    domains = json.loads((ROOT / "private/domains.json").read_text())
    if not all(h.endswith(".acceptance.invalid") for h in domains.values()):
        raise SystemExit("refusing non-test domains")
    baseline = ROOT / "private/e2e/result.json"
    if (
        not baseline.is_file()
        or json.loads(baseline.read_text()).get("result") != "passed"
    ):
        raise SystemExit("run fresh-stack.sh successfully first")
    edge = ROOT / "private/e2e/edge"
    target = Path("/app/open-assets-traefik")
    target.mkdir(mode=0o755)
    (target / "dynamic").mkdir(mode=0o750)
    for directory in (target, target / "dynamic"):
        directory.chmod(0o750)
        os.chown(directory, 0, 20003)
    (target / "acme").mkdir(mode=0o700)
    os.chown(target / "acme", 20003, 20003)
    (target / "acme/acme.json").touch(mode=0o600)
    os.chown(target / "acme/acme.json", 20003, 20003)
    shutil.copyfile(
        ROOT / "config/traefik/compose.example.yaml", target / "compose.yaml"
    )
    shutil.copyfile(
        ROOT / "config/traefik/traefik.example.yaml", target / "traefik.yaml"
    )
    (target / "traefik.yaml").chmod(0o644)
    # Keep the shipped static configuration intact. Fixture routes use a private
    # file certificate: .invalid domains cannot receive public ACME certificates.
    for name in ("routes.yaml", "tls.yaml"):
        shutil.copyfile(edge / "dynamic" / name, target / "dynamic" / name)
        (target / "dynamic" / name).chmod(0o644)
    compose = yaml.safe_load((target / "compose.yaml").read_text())
    compose["services"]["traefik"]["volumes"] += [
        str(edge / "edge.crt") + ":/etc/traefik/edge.crt:ro",
        str(edge / "edge.key") + ":/etc/traefik/edge.key:ro",
    ]
    (target / "compose.yaml").write_text(yaml.safe_dump(compose))
    command("docker", "compose", "-f", str(edge / "compose.yaml"), "down")
    prefix = (
        "docker",
        "compose",
        "--env-file",
        str(ROOT / ".env"),
        "-f",
        str(target / "compose.yaml"),
    )
    command(*prefix, "config", "--quiet")
    command(*prefix, "up", "-d")
    identifier = command(*prefix, "ps", "-q", "traefik").decode().strip()
    for _ in range(60):
        state = json.loads(command("docker", "inspect", identifier))[0]["State"]
        if state.get("Health", {}).get("Status") == "healthy":
            break
        time.sleep(1)
    else:
        raise RuntimeError("new Traefik template healthcheck did not pass")
    context = ssl.create_default_context(cafile=str(ROOT / "private/tls/ca.crt"))
    opener = urllib.request.build_opener(urllib.request.HTTPSHandler(context=context))

    def request(url, data=None, method=None, headers=None, client=opener):
        try:
            with client.open(
                urllib.request.Request(
                    url, data=data, method=method, headers=headers or {}
                ),
                timeout=20,
            ) as response:
                return response.status, response.read(), response.headers
        except urllib.error.HTTPError as error:
            return error.code, error.read(), error.headers

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            return None

    redirect_client = urllib.request.build_opener(NoRedirect())
    code, _, headers = request(
        "http://" + domains["oss"] + "/not-found", client=redirect_client
    )
    assert code in (301, 302, 307, 308)
    location = urllib.parse.urlparse(headers["Location"])
    assert (
        location.scheme,
        location.hostname,
        location.port or 443,
        location.path,
    ) == (
        "https",
        domains["oss"],
        443,
        "/not-found",
    )
    assert opener.open("https://" + domains["auth"] + "/healthz").status == 200
    for name in ("storage", "git", "traefik"):
        response = opener.open("https://" + domains[name] + "/")
        assert urllib.parse.urlparse(response.url).hostname == domains["auth"]
        response.close()
    before = snapshot()
    # Execute the administrator snippets from the documentation, rather than a
    # separately maintained copy of its policy or route generator.
    blocks = re.findall(
        r"```sh\n(.*?)```", (ROOT / "docs/web-uploads.md").read_text(), re.S
    )
    setup = blocks[0].split("set -a", 1)[1]
    setup = "set -eu\ncd /app/open-assets\nset -a" + setup
    policy_before = command(
        "sh", "-c", setup + "\noc_admin policy get-json store/attachments"
    )
    command("sh", "-c", setup + "\n" + blocks[1])
    repeated = subprocess.run(
        ["sh", "-c", setup + "\n" + blocks[1]], capture_output=True
    )
    assert repeated.returncode != 0
    assert policy_before == command(
        "sh", "-c", setup + "\noc_admin policy get-json store/attachments"
    )
    command("sh", "-c", blocks[2])
    route = target / "dynamic/open-assets-uploads.yaml"
    assert route.stat().st_mode & 0o777 == 0o644
    original = route.read_bytes()
    repeated = subprocess.run(["sh", "-c", blocks[2]], capture_output=True)
    assert repeated.returncode != 0 and original == route.read_bytes()
    data = json.loads(original)
    data["http"]["routers"]["uploads-public"]["tls"] = {}
    route.write_text(json.dumps(data))
    route.chmod(0o644)
    time.sleep(3)
    jar = http.cookiejar.CookieJar()
    browser = urllib.request.build_opener(
        urllib.request.HTTPSHandler(context=context),
        urllib.request.HTTPCookieProcessor(jar),
    )
    origin = "https://" + domains["storage"]
    password = (ROOT / "private/stargate_password").read_text().strip()
    login = urllib.parse.urlencode(
        {"password": password, "callback": domains["storage"]}
    ).encode()
    code, _, _ = request(
        "https://" + domains["auth"] + "/_login", login, client=browser
    )
    assert code == 200

    def api(path, data=None, method=None, headers=None):
        return request(
            origin + path, data, method, {"Origin": origin, **(headers or {})}, browser
        )

    code, body, _ = api(
        "/api/login",
        json.dumps(
            {
                "accessKey": (ROOT / "private/console_user").read_text().strip(),
                "secretKey": (ROOT / "private/console_password").read_text().strip(),
            }
        ).encode(),
        headers={"Content-Type": "application/json"},
    )
    assert code == 200
    csrf = json.loads(body)["csrfToken"]
    assert api("/api/logout", b"", "POST")[0] == 403
    content = b"Hello Open Assets\n"
    keys = ["public/hello.txt", "public/中文 + %.txt", "private/hidden.txt"]
    payloads = dict.fromkeys(keys, content)
    payloads["public/diagram.svg"] = (
        b'<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>'
    )
    for key, payload in payloads.items():
        code, body, _ = api(
            "/api/uploads",
            json.dumps(
                {
                    "bucket": "uploads",
                    "key": key,
                    "size": len(payload),
                    "overwrite": False,
                }
            ).encode(),
            headers={"Content-Type": "application/json", "X-CSRF-Token": csrf},
        )
        assert code in (200, 201, 202)
        upload = json.loads(body)["id"]
        assert api(
            "/api/uploads/" + upload,
            payload,
            "PUT",
            {"Content-Type": "application/octet-stream", "X-CSRF-Token": csrf},
        )[0] in (200, 201, 202)
    for key in [*keys[:2], "public/diagram.svg"]:
        url = "https://" + domains["oss"] + "/uploads/" + urllib.parse.quote(key)
        for _ in range(30):
            code, body, _ = request(url)
            if code == 200:
                break
            time.sleep(1)
        assert code == 200 and body == payloads[key]
        assert request(url, method="HEAD")[0] == 200
        code, body, _ = request(url, headers={"Range": "bytes=0-4"})
        assert code == 206 and body == payloads[key][:5]
    public = "https://" + domains["oss"]
    for path, method, body in [
        ("/uploads?list-type=2", "GET", None),
        ("/uploads/private/hidden.txt", "GET", None),
        ("/uploads/public/new.txt", "PUT", b"no"),
        ("/uploads/public/hello.txt", "DELETE", None),
    ]:
        assert request(public + path, body, method)[0] not in range(200, 300)
    # Also test the storage policy directly, independently from route filtering.
    with Path("/etc/hosts").open("a") as hosts:
        hosts.write("\n127.0.0.1 storage.internal\n")
    direct = "https://storage.internal:19000"
    assert request(direct + "/uploads/public/hello.txt")[1] == content
    for path, method, body in [
        ("/uploads?list-type=2", "GET", None),
        ("/uploads/private/hidden.txt", "GET", None),
        ("/uploads/public/new.txt", "PUT", b"no"),
        ("/uploads/public/hello.txt", "DELETE", None),
    ]:
        assert request(direct + path, body, method)[0] == 403
    assert request(direct + "/uploads/public/hello.txt")[1] == content
    assert api("/api/logout", b"", "POST", {"X-CSRF-Token": csrf})[0] in (200, 204)
    assert api("/api/buckets")[0] == 401
    moved = target / "uploads-disabled.json"
    route.rename(moved)
    time.sleep(3)
    assert request(public + "/uploads/public/hello.txt")[0] == 404
    moved.rename(route)
    time.sleep(3)
    assert request(public + "/uploads/public/hello.txt")[1] == content
    assert before == snapshot(), "web upload changed Git publication state"
    report = {
        "passed": True,
        "checks": [
            "SVG object upload and anonymous byte verification (not browser preview)",
            "template-health",
            "redirect",
            "management-auth",
            "documented-bucket-policy",
            "repeat-refusal",
            "oc-upload-download-logout",
            "special-path-range",
            "anonymous-policy-boundaries",
            "route-withdrawal",
            "git-state-unchanged",
        ],
        "not_tested": [
            "public DNS and ACME issuance",
            "browser UI",
            "Ubuntu package installation",
        ],
    }
    (ROOT / "private/e2e/community-result.json").write_text(
        json.dumps(report, indent=2)
    )
    print("PASS community Traefik template and web sharing acceptance")


if __name__ == "__main__":
    main()
