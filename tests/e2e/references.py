"""Reference URL and delta acceptance; only run after the disposable fresh stack."""

import json
import os
from pathlib import Path
import ssl
import sys
import time
import urllib.parse
import urllib.request

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from verify import command, wait_job  # noqa: E402


def main():
    domains = json.loads((ROOT / "private/domains.json").read_text())
    if (
        os.environ.get("OPEN_ASSETS_FRESH_TEST") != "1"
        or os.geteuid() != 0
        or not all(h.endswith(".acceptance.invalid") for h in domains.values())
    ):
        raise SystemExit("requires the isolated fresh-stack fixture")
    config_path = ROOT / "private/repositories.yaml"
    command("docker", "compose", "stop", "publisher")
    config = yaml.safe_load(config_path.read_text())
    config.setdefault("publication", {})["reference_releases"] = True
    config_path.write_text(yaml.safe_dump(config, sort_keys=False))
    command(
        "docker",
        "compose",
        "up",
        "-d",
        "--no-build",
        "--force-recreate",
        "publisher",
        "object-reader",
    )
    routes_path = ROOT / "private/e2e/edge/dynamic/routes.yaml"
    if os.environ.get("OPEN_ASSETS_COMMUNITY_TEST") == "1":
        # community.py replaces the fixture edge with the shipped template.
        routes_path = Path("/app/open-assets-traefik/dynamic/routes.yaml")
    routes = json.loads(routes_path.read_text())
    routes["http"]["services"]["release-objects"] = {
        "loadBalancer": {"servers": [{"url": "http://object-reader:8083"}]}
    }
    routes["http"]["routers"]["oss"]["service"] = "release-objects"
    routes_path.write_text(json.dumps(routes))
    time.sleep(3)
    checkout = ROOT / "private/e2e/images"
    env = dict(
        os.environ,
        GIT_SSH_COMMAND=f"ssh -i {ROOT}/private/e2e/personal-key -o UserKnownHostsFile={ROOT}/private/e2e/known_hosts -o StrictHostKeyChecking=yes -o BatchMode=yes",
    )

    def git(*args):
        return command("git", "-C", str(checkout), *args, env=env).decode().strip()

    def push(message):
        git("add", "public")
        git("commit", "--allow-empty", "-m", message)
        sha = git("rev-parse", "HEAD")
        git("push", "origin", "main")
        wait_job("main-images", sha)
        return sha

    git("pull", "--ff-only")
    first = push("reference format warmup")
    image = checkout / "public/images/reference-test.svg"
    image.write_bytes(
        b'<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>'
    )
    second = push("reference delta")
    manifest = json.loads(
        (
            Path("/data/asset-stack/publisher/manifests/main-images")
            / (second + ".json")
        ).read_text()
    )
    assert manifest["object_sources"]
    jobs = [
        json.loads(p.read_text())
        for p in Path("/data/asset-stack/publisher/jobs").glob("*.json")
    ]
    job = next(j for j in jobs if j["sha"] == second)
    assert job["incremental"]["object_transfers"]["uploaded"] == 1
    assert (
        job["incremental"]["object_transfers"]["referenced"]
        == len(manifest["files"]) - 1
    )
    context = ssl.create_default_context(cafile=str(ROOT / "private/tls/ca.crt"))
    path = "documents/manual.txt"
    data = (checkout / "public" / path).read_bytes()
    for sha in (first, second):
        url = f"https://{domains['oss']}/attachments/repositories/main-images/releases/{sha}/{path}"
        with urllib.request.urlopen(url, context=context) as response:
            assert response.status == 200 and response.read() == data
            etag = response.headers["ETag"]
        request = urllib.request.Request(url, headers={"Range": "bytes=0-7"})
        with urllib.request.urlopen(request, context=context) as response:
            assert response.status == 206 and response.read() == data[:8]
        request = urllib.request.Request(url, method="HEAD")
        with urllib.request.urlopen(request, context=context) as response:
            assert response.status == 200 and int(
                response.headers["Content-Length"]
            ) == len(data)
        try:
            urllib.request.urlopen(
                urllib.request.Request(url, headers={"If-None-Match": etag}),
                context=context,
            )
        except urllib.error.HTTPError as error:
            assert error.code == 304
            error.close()
        else:
            raise AssertionError("conditional GET lost")
    command(
        "docker",
        "compose",
        "exec",
        "-T",
        "publisher",
        "python",
        "-m",
        "open_assets",
        "rollback-images",
        "main-images",
        first,
    )
    command(
        "docker",
        "compose",
        "exec",
        "-T",
        "publisher",
        "python",
        "-m",
        "open_assets",
        "rollback-images",
        "main-images",
        second,
    )
    print(
        "PASS reference delta, old/new URLs, streaming Range/HEAD/304 and image rollback"
    )


if __name__ == "__main__":
    main()
