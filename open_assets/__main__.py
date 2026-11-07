"""Signed event receiver and single-host Git asset publisher.

The host administration CLI lives in admin.py. This module runs without Docker
or storage-admin access; publication uses only the configured repository and S3
identity. Task recovery is replay-based, not a cross-system transaction.
"""

import argparse
import contextlib
import fcntl
import hashlib
import hmac
import http.server
import json
import mimetypes
import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import threading
import time
import uuid
import yaml

P = pathlib.Path
CONFIG = os.environ.get("OPEN_ASSETS_CONFIG", "/config/repositories.yaml")
SHA = re.compile(r"^[0-9a-f]{40}$")


def atomic(path, value):
    """Replace one JSON record after syncing its bytes and parent directory."""
    path = P(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent)
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(value, f, ensure_ascii=False)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
        d = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(d)
        finally:
            os.close(d)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def load():
    """Read and validate the shared configuration; paths are container paths."""
    raw = P(CONFIG).read_bytes()
    c = yaml.safe_load(raw)
    if c.get("schema_version") != 1:
        raise ValueError("unsupported schema")
    keys = set()
    ids = set()
    mounts = []
    for r in c["repositories"]:
        if not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", r["key"]):
            raise ValueError("invalid key")
        if r["key"] in keys or r["repository_id"] in ids:
            raise ValueError("duplicate repository")
        if type(r["repository_id"]) is not int or r["repository_id"] < 1:
            raise ValueError("invalid ID")
        keys.add(r["key"])
        ids.add(r["repository_id"])
        for field in ["source_directory"]:
            p = P(r[field])
            if p.is_absolute() or ".." in p.parts or str(p) == ".":
                raise ValueError("invalid source")
        m = r["mount_path"]
        if not re.fullmatch(r"/(?:[a-z0-9][a-z0-9-]*/)?", m):
            raise ValueError("invalid mount")
        if m in mounts or any(
            m != "/" and x != "/" and (m.startswith(x) or x.startswith(m))
            for x in mounts
        ):
            raise ValueError("mount collision")
        mounts.append(m)
        if not r["remote"].startswith("ssh://git@"):
            raise ValueError("SSH remote required")
        if (
            not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._/-]*", r["branch"])
            or ".." in r["branch"]
        ):
            raise ValueError("invalid branch")
    c["revision"] = hashlib.sha256(raw).hexdigest()
    return c


def repo(c, key):
    r = next(x for x in c["repositories"] if x["key"] == key)
    if not r.get("enabled", False):
        raise ValueError("repository disabled")
    return r


def git(path, *args):
    return subprocess.check_output(
        ["git", "--git-dir", str(path), *args], stderr=subprocess.PIPE, timeout=120
    )


def enqueue(c, key, sha, delivery, kind="auto"):
    """Deduplicate by repository/SHA/kind without resetting an existing failure."""
    repo(c, key)
    if not SHA.fullmatch(sha) or sha == "0" * 40:
        raise ValueError("invalid commit")
    jid = hashlib.sha256((key + ":" + sha + ":" + kind).encode()).hexdigest()
    path = P(c["state_root"]) / "jobs" / f"{jid}.json"
    if not path.exists():
        atomic(
            path,
            dict(
                id=jid,
                key=key,
                sha=sha,
                delivery=str(delivery)[:128],
                kind=kind,
                status="pending",
                created=time.time(),
                config=c["revision"],
            ),
        )
    return jid


@contextlib.contextmanager
def lock(c, name="worker"):
    """Use separate queue/worker locks so uploads do not block durable enqueue."""
    p = P(c["state_root"])
    p.mkdir(parents=True, exist_ok=True)
    with (p / (name + ".lock")).open("a") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        yield


def s3client(c):
    import boto3
    from botocore.config import Config

    def secret(name):
        return P(os.environ[name + "_FILE"]).read_text().strip()

    return boto3.client(
        "s3",
        endpoint_url=c["endpoint"],
        region_name="us-east-1",
        aws_access_key_id=secret("S3_ACCESS_KEY"),
        aws_secret_access_key=secret("S3_SECRET_KEY"),
        verify=os.environ.get("S3_CA_FILE", True),
        config=Config(
            signature_version="s3v4",
            s3={"addressing_style": "path"},
            request_checksum_calculation="when_required",
            response_checksum_validation="when_required",
        ),
    )


def export(c, r, g, sha, dest):
    """Read committed blobs directly; do not checkout or execute repository code.

    Image checks use extensions, not file signatures. Other public files are
    attachments and remain subject to the same path and size limits.
    """
    entries = git(g, "ls-tree", "-rz", sha, "--", r["source_directory"]).split(b"\0")
    files = []
    total = 0
    reserved = {
        x["mount_path"].strip("/") for x in c["repositories"] if x["mount_path"] != "/"
    }
    for entry in entries:
        if not entry:
            continue
        meta, name = entry.split(b"\t", 1)
        mode, typ, oid = meta.split()
        if mode not in (b"100644", b"100755") or typ != b"blob":
            raise ValueError("unsupported Git entry")
        path = name.decode("utf-8")
        rel = P(path).relative_to(r["source_directory"])
        if ".." in rel.parts or any(x.startswith(".") for x in rel.parts):
            raise ValueError("unsafe path")
        if (
            r["mount_path"] == "/"
            and len(rel.parts) > 1
            and rel.parts[0] == "images"
            and rel.parts[1] in reserved
        ):
            raise ValueError("reserved URL prefix")
        size = int(git(g, "cat-file", "-s", oid.decode()))
        if size > r.get("max_file_bytes", 104857600):
            raise ValueError("file limit exceeded")
        data = git(g, "cat-file", "blob", oid.decode())
        if data.startswith(b"version https://git-lfs.github.com/spec/v1"):
            raise ValueError("LFS unsupported")
        total += len(data)
        if (
            len(data) > r.get("max_file_bytes", 104857600)
            or total > r.get("max_bytes", 1073741824)
            or len(files) >= r.get("max_files", 10000)
        ):
            raise ValueError("publication limit exceeded")
        if rel.parts[0] == "images" and rel.suffix.lower() not in {
            ".png",
            ".jpg",
            ".jpeg",
            ".gif",
            ".webp",
            ".avif",
            ".ico",
        }:
            raise ValueError("image format not allowed")
        out = dest / rel
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(data)
        with out.open("rb") as syncfile:
            os.fsync(syncfile.fileno())
        files.append(
            dict(
                path=rel.as_posix(),
                size=len(data),
                sha256=hashlib.sha256(data).hexdigest(),
                content_type=mimetypes.guess_type(path)[0]
                or "application/octet-stream",
            )
        )
    if not files:
        raise ValueError("empty publication")
    return files


def sync_tree(root):
    """Sync a prepared tree before exposing it through a version-directory rename."""
    for directory, _, files in os.walk(root, topdown=False):
        for name in files:
            with (P(directory) / name).open("rb") as f:
                os.fsync(f.fileno())
        fd = os.open(directory, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)


def switch(base, target):
    """Replace current on the same filesystem; this does not commit S3 or jobs."""
    tmp = base / (".current-" + uuid.uuid4().hex)
    os.symlink("releases/" + target, tmp)
    os.replace(tmp, base / "current")
    d = os.open(base, os.O_RDONLY)
    try:
        os.fsync(d)
    finally:
        os.close(d)


def publish(c, j):
    """Replay a publication, checking existing immutable bytes before activation.

    A failure may leave uploaded objects or staging data. Never compensate by
    deleting them: a preceding attempt may already have made them readable.
    """
    r = repo(c, j["key"])
    state = P(c["state_root"])
    g = state / "git" / r["key"]
    g.parent.mkdir(parents=True, exist_ok=True)
    if not g.exists():
        subprocess.run(
            ["git", "init", "--bare", str(g)], check=True, stdout=subprocess.DEVNULL
        )
    git(
        g,
        "fetch",
        "--no-tags",
        r["remote"],
        "+refs/heads/" + r["branch"] + ":refs/heads/source",
    )
    head = git(g, "rev-parse", "refs/heads/source").decode().strip()
    if j["kind"] == "auto" and head != j["sha"]:
        j["status"] = "superseded"
        return
    git(g, "merge-base", "--is-ancestor", j["sha"], head)
    if shutil.disk_usage(c["images_root"]).free < r.get("max_bytes", 1073741824) * 2:
        raise ValueError("insufficient staging capacity")
    base = P(c["images_root"]) / "repos" / r["key"]
    (base / "releases").mkdir(parents=True, exist_ok=True)
    staging = P(tempfile.mkdtemp(prefix=".staging-", dir=base))
    try:
        files = export(c, r, g, j["sha"], staging)
        s = s3client(c)
        prefix = f"repositories/{r['key']}/releases/{j['sha']}/"
        for item in files:
            data = (staging / item["path"]).read_bytes()
            key = prefix + item["path"]
            try:
                # ETag is not a content hash for all S3 upload modes. Compare bytes.
                existing = s.get_object(Bucket=c["bucket"], Key=key)["Body"].read()
            except s.exceptions.NoSuchKey:
                existing = None
            if (
                existing is not None
                and hashlib.sha256(existing).hexdigest() != item["sha256"]
            ):
                raise ValueError("immutable object conflict")
            if existing is None:
                s.put_object(
                    Bucket=c["bucket"],
                    Key=key,
                    Body=data,
                    ContentType=item["content_type"],
                    CacheControl="public, max-age=31536000, immutable",
                    Metadata={"sha256": item["sha256"]},
                )
            actual = s.get_object(Bucket=c["bucket"], Key=key)["Body"].read()
            if hashlib.sha256(actual).hexdigest() != item["sha256"]:
                raise ValueError("object verification failed")
        manifest = dict(key=r["key"], sha=j["sha"], files=files, config=c["revision"])
        atomic(state / "manifests" / r["key"] / (j["sha"] + ".json"), manifest)
        final = base / "releases" / j["sha"]
        if final.exists():
            for item in files:
                if (
                    hashlib.sha256((final / item["path"]).read_bytes()).hexdigest()
                    != item["sha256"]
                ):
                    raise ValueError("local immutable conflict")
        else:
            sync_tree(staging)
            os.rename(staging, final)
        # Keep the complete public snapshot and a separate image-serving subset.
        image_final = base / "releases" / ("images-" + j["sha"])
        if not (final / "images").is_dir():
            raise ValueError("missing images directory")
        if not image_final.exists():
            image_tmp = P(tempfile.mkdtemp(prefix=".images-", dir=base))
            shutil.copytree(final / "images", image_tmp, dirs_exist_ok=True)
            for f in image_tmp.rglob("*"):
                if f.is_file():
                    with f.open("rb") as syncfile:
                        os.fsync(syncfile.fileno())
            sync_tree(image_tmp)
            os.rename(image_tmp, image_final)
        # Recheck before activation. A Push can still race after this check;
        # Git head and the filesystem link do not share a commit boundary.
        git(
            g,
            "fetch",
            "--no-tags",
            r["remote"],
            "+refs/heads/" + r["branch"] + ":refs/heads/source",
        )
        if (
            j["kind"] == "auto"
            and git(g, "rev-parse", "refs/heads/source").decode().strip() != j["sha"]
        ):
            j["status"] = "superseded"
            return
        previous = (
            os.readlink(base / "current") if (base / "current").is_symlink() else None
        )
        j.update(status="images_prepared", previous=previous)
        atomic(state / "jobs" / (j["id"] + ".json"), j)
        switch(base, "images-" + j["sha"])
        # Validate activated bytes through the same path Nginx mounts.
        for item in files:
            if (
                item["path"].startswith("images/")
                and hashlib.sha256(
                    (base / "current" / item["path"][7:]).read_bytes()
                ).hexdigest()
                != item["sha256"]
            ):
                if previous:
                    switch(base, P(previous).name)
                raise ValueError("activation verification failed")
        j["status"] = "published"
        j["finished"] = time.time()
        atomic(state / "published" / r["key"], manifest)
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def work_once(c, heartbeat=None):
    """Resume interrupted nonterminal work; failed jobs require an explicit retry."""
    with lock(c):
        jobs = P(c["state_root"]) / "jobs"
        jobs.mkdir(parents=True, exist_ok=True)
        for p in sorted(
            jobs.glob("*.json"), key=lambda p: json.loads(p.read_text())["created"]
        ):
            j = json.loads(p.read_text())
            if j["status"] not in ["pending", "running", "images_prepared"]:
                continue
            j["status"] = "running"
            atomic(p, j)
            if heartbeat is not None:
                heartbeat.update(job_started_at=time.time(), progress_at=time.time())
            try:
                publish(c, j)
            except Exception as e:
                j.update(status="failed", error=type(e).__name__)
            atomic(p, j)
            if heartbeat is not None:
                heartbeat.update(job_started_at=None, progress_at=time.time())


def render(c, directory):
    """Write a candidate Nginx file; the caller owns validation and reload."""
    directory = P(directory)
    directory.mkdir(parents=True, exist_ok=True)
    lines = [
        "include /etc/nginx/mime.types;",
        "default_type application/octet-stream;",
        "server { listen 8080; server_tokens off; autoindex off; open_file_cache off;",
        "add_header X-Content-Type-Options nosniff always;",
        'add_header Cache-Control "public, max-age=300, must-revalidate";',
        "location ~ (^|/)\\. { deny all; }",
    ]
    for r in c["repositories"]:
        if not r.get("serve", True):
            continue
        mount = r["mount_path"]
        root = "/srv/images/repos/" + r["key"] + "/current/"
        if mount == "/":
            lines.append(
                f"location / {{ root {root}; limit_except GET HEAD {{ deny all; }} try_files $uri =404; }}"
            )
        else:
            lines.append(
                f"location {mount} {{ alias {root}; limit_except GET HEAD {{ deny all; }} }}"
            )
    if not any(
        r["mount_path"] == "/" and r.get("serve", True) for r in c["repositories"]
    ):
        lines.append("location / { return 404; }")
    lines.append("}")
    (directory / "images.conf").write_text("\n".join(lines) + "\n")


class Handler(http.server.BaseHTTPRequestHandler):
    """Internal Gitea endpoint; a 202 response confirms enqueue, not publication."""

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200 if self.path == "/healthz" else 404)
        self.end_headers()

    def do_POST(self):
        try:
            self.connection.settimeout(15)
            c = load()
            size = int(self.headers.get("Content-Length", "0"))
            if self.path != "/hooks/publish-assets" or not 0 < size <= 1048576:
                raise ValueError()
            # Signatures cover original bytes; parse JSON only after verification.
            body = self.rfile.read(size)
            secret = P(os.environ["WEBHOOK_SECRET_FILE"]).read_bytes().strip()
            expected = hmac.new(secret, body, hashlib.sha256).hexdigest()
            if not hmac.compare_digest(
                expected, self.headers.get("X-Gitea-Signature", "")
            ):
                raise ValueError()
            if self.headers.get("X-Gitea-Event") != "push":
                raise ValueError()
            payload = json.loads(body)
            # Identity comes from the ID allowlist, never payload clone URLs.
            r = next(
                r
                for r in c["repositories"]
                if r["repository_id"] == payload["repository"]["id"]
            )
            if payload["ref"] != "refs/heads/" + r["branch"]:
                raise ValueError()
            with lock(c, "queue"):
                jid = enqueue(
                    c,
                    r["key"],
                    payload["after"],
                    self.headers.get("X-Gitea-Delivery", ""),
                )
            # enqueue() has fsynced the task and directory before acknowledging.
            self.send_response(202)
            self.end_headers()
            self.wfile.write(json.dumps({"job": jid}).encode())
        except (ValueError, KeyError, StopIteration, json.JSONDecodeError):
            self.send_response(403)
            self.end_headers()
        except Exception:
            self.send_response(503)
            self.end_headers()


def main():
    a = argparse.ArgumentParser()
    a.add_argument(
        "command",
        choices=[
            "validate",
            "render",
            "serve",
            "worker",
            "once",
            "publish",
            "retry",
            "rollback-images",
            "status",
        ],
    )
    a.add_argument("args", nargs="*")
    o = a.parse_args()
    c = load()
    if o.command == "validate":
        print("configuration valid")
    elif o.command == "render":
        render(c, o.args[0])
    elif o.command == "serve":
        http.server.ThreadingHTTPServer(("0.0.0.0", 9000), Handler).serve_forever()
    elif o.command == "worker":
        heartbeat = dict(
            updated_at=time.time(), progress_at=time.time(), job_started_at=None
        )
        stop = threading.Event()

        def beat():
            while not stop.is_set():
                heartbeat["updated_at"] = time.time()
                atomic(P(c["state_root"]) / "worker-heartbeat.json", dict(heartbeat))
                stop.wait(10)

        threading.Thread(target=beat, daemon=True).start()
        try:
            while True:
                heartbeat["progress_at"] = time.time()
                work_once(load(), heartbeat)
                time.sleep(2)
        finally:
            stop.set()
    elif o.command == "once":
        work_once(c)
    elif o.command == "publish":
        with lock(c, "queue"):
            print(enqueue(c, *o.args, "manual", kind="manual"))
    elif o.command == "retry":
        if not re.fullmatch("[0-9a-f]{64}", o.args[0]):
            raise ValueError("invalid job ID")
        with lock(c):
            p = P(c["state_root"]) / "jobs" / (o.args[0] + ".json")
            j = json.loads(p.read_text())
            if j["status"] != "failed":
                raise ValueError("only failed jobs may retry")
            j["status"] = "pending"
            atomic(p, j)
    elif o.command == "rollback-images":
        key, sha = o.args
        repo(c, key)
        if not SHA.fullmatch(sha):
            raise ValueError("invalid SHA")
        with lock(c):
            manifest = json.loads(
                (P(c["state_root"]) / "manifests" / key / (sha + ".json")).read_text()
            )
            base = P(c["images_root"]) / "repos" / key
            for f in manifest["files"]:
                if (
                    hashlib.sha256(
                        (base / "releases" / sha / f["path"]).read_bytes()
                    ).hexdigest()
                    != f["sha256"]
                ):
                    raise ValueError("rollback bytes mismatch")
            switch(base, "images-" + sha)
            atomic(P(c["state_root"]) / "published" / key, manifest)
    else:
        for p in (P(c["state_root"]) / "jobs").glob("*.json"):
            j = json.loads(p.read_text())
            print(j["id"], j["key"], j["sha"], j["status"])


if __name__ == "__main__":
    main()
