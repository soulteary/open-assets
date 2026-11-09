"""Signed event receiver and single-host Git asset publisher.

The host administration CLI lives in admin.py. This module runs without Docker
or storage-admin access; publication uses only the configured repository and S3
identity. Task recovery is replay-based, not a cross-system transaction.
"""

import contextlib
import fcntl
import hashlib
import hmac
import http.server
import json
import itertools
import mimetypes
import math
import os
import pathlib
import re
import shutil
import stat
import sys
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


def load(path=None):
    """Read and validate the shared configuration; paths are container paths."""
    raw = P(path or CONFIG).read_bytes()
    c = yaml.safe_load(raw)
    from .resources import policy
    from .publication import policy as publication_policy

    policy(c)
    publication_policy(c)
    if c.get("schema_version") != 1:
        raise ValueError("unsupported schema")
    keys = set()
    ids = set()
    mounts = []
    for r in c["repositories"]:
        for flag in ("enabled", "serve"):
            if flag in r and type(r[flag]) is not bool:
                raise ValueError("repository flags must be YAML booleans")
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
    from .retention import policy

    for r in c["repositories"]:
        policy(c, r)
        from .object_retention import policy as object_policy

        object_policy(c, r)
    c["revision"] = hashlib.sha256(raw).hexdigest()
    return c


def repo(c, key):
    r = next((x for x in c["repositories"] if x["key"] == key), None)
    if r is None:
        raise ValueError("unknown repository key")
    if not r.get("enabled", False):
        raise ValueError("repository disabled")
    return r


def git(path, *args, c=None):
    from .resources import run_git

    return run_git(path, list(args), c)


def enqueue(c, key, sha, delivery, kind="auto"):
    with lock(c, "queue"):
        return _enqueue(c, key, sha, delivery, kind)


def _enqueue(c, key, sha, delivery, kind="auto"):
    """Deduplicate by repository/SHA/kind without resetting an existing failure."""
    repo(c, key)
    if not SHA.fullmatch(sha) or sha == "0" * 40:
        raise ValueError("invalid commit")
    jid = hashlib.sha256((key + ":" + sha + ":" + kind).encode()).hexdigest()
    path = P(c["state_root"]) / "jobs" / f"{jid}.json"
    from .retention import archive_path

    archived = archive_path(c, jid)
    if (
        not path.exists()
        and not path.is_symlink()
        and not archived.exists()
        and not archived.is_symlink()
    ):
        from .resources import admit_queue

        try:
            admit_queue(c)
        except Exception as error:
            from .resource_events import record

            record(c, error, key, category="queue")
            raise
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
def lock(c, name="worker", blocking=True):
    """Use separate queue/worker locks so uploads do not block durable enqueue."""
    p = P(c["state_root"])
    p.mkdir(parents=True, exist_ok=True)
    with (p / (name + ".lock")).open("a") as f:
        fcntl.flock(f, fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
        yield


def s3client(c, bounded=False):
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
            **(
                dict(
                    connect_timeout=5,
                    read_timeout=15,
                    retries={"total_max_attempts": 1},
                )
                if bounded
                else {}
            ),
        ),
    )


def export(c, r, g, sha, dest, check=None, reuse=None):
    """Read committed blobs directly; do not checkout or execute repository code.

    Image checks use extensions, not file signatures. Other public files are
    attachments and remain subject to the same path and size limits.
    """
    from .resources import run_git
    from .git_batch import GitBatch

    entries = run_git(
        g, ["ls-tree", "-rlz", sha, "--", r["source_directory"]], c, check=check
    ).split(b"\0")
    files = []
    blobs = []
    total = 0
    reserved = {
        x["mount_path"].strip("/") for x in c["repositories"] if x["mount_path"] != "/"
    }
    for entry in entries:
        if check is not None:
            check()
        if not entry:
            continue
        meta, name = entry.split(b"\t", 1)
        mode, typ, oid, raw_size = meta.split()
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
        size = int(raw_size)
        if size < 0:
            raise ValueError("invalid Git blob size")
        if size > r.get("max_file_bytes", 104857600):
            raise ValueError("file limit exceeded")
        if rel.suffix.lower() == ".svg":
            from .svg import MAX_BYTES

            if size > MAX_BYTES:
                raise ValueError("SVG size limit exceeded")
        total += size
        if total > r.get("max_bytes", 1073741824) or len(blobs) >= r.get(
            "max_files", 10000
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
            ".svg",
        }:
            raise ValueError("image format not allowed")
        blobs.append((oid, rel, path, size))
        if reuse is not None:
            reuse["oids"][rel.as_posix()] = oid.decode()
    prepared_parents = set()
    # Reject all unsafe paths, types and size budgets before reading any blob.
    with GitBatch(g, c, check=check) as batch:
        for oid, rel, path, size in blobs:
            out = dest / rel
            if out.parent not in prepared_parents:
                out.parent.mkdir(parents=True, exist_ok=True)
                prepared_parents.add(out.parent)
            if reuse is not None:
                from .incremental import copy_cached

                cached = copy_cached(reuse, oid, size, rel, out)
                if cached is not None:
                    if rel.suffix.lower() == ".svg":
                        from .svg import validate

                        validate(out)
                    cached["content_type"] = (
                        mimetypes.guess_type(path)[0] or "application/octet-stream"
                    )
                    files.append(cached)
                    continue
            with out.open("xb") as stream:
                batch.write_blob(oid, size, stream)
                stream.flush()
                os.fsync(stream.fileno())
            if rel.suffix.lower() == ".svg":
                from .svg import validate

                validate(out)
            digest = hashlib.sha256()
            actual_size = 0
            with out.open("rb") as stream:
                first = True
                while block := stream.read(1024 * 1024):
                    if check is not None:
                        check()
                    if first and block.startswith(
                        b"version https://git-lfs.github.com/spec/v1"
                    ):
                        raise ValueError("LFS unsupported")
                    first = False
                    actual_size += len(block)
                    digest.update(block)
            if actual_size != size:
                raise ValueError("Git blob size mismatch")
            files.append(
                dict(
                    path=rel.as_posix(),
                    size=size,
                    sha256=digest.hexdigest(),
                    content_type=mimetypes.guess_type(path)[0]
                    or "application/octet-stream",
                )
            )
            if reuse is not None and reuse.get("reference_mode"):
                from . import immutable

                reuse["verified"][rel.as_posix()] = immutable.record(out, files[-1])
    if not files:
        raise ValueError("empty publication")
    return files


def sync_tree(root, data_synced=False):
    """Sync a prepared tree before exposing it through a version-directory rename."""
    if data_synced:
        from concurrent.futures import ThreadPoolExecutor
        from .retention import sync_directory

        levels = {}
        entries = 0
        for directory, _, names in os.walk(root, topdown=False):
            entries += 1 + len(names)
            if entries > 200000:
                raise ValueError("snapshot sync entry budget exceeded")
            depth = len(P(directory).relative_to(root).parts)
            levels.setdefault(depth, []).append(directory)
        # Sibling directories can share a journal commit; ancestors must wait.
        with ThreadPoolExecutor(max_workers=4) as pool:
            for depth in sorted(levels, reverse=True):
                directories = levels[depth]
                for offset in range(0, len(directories), 32):
                    for _ in pool.map(
                        sync_directory, directories[offset : offset + 32]
                    ):
                        pass
        return
    for directory, _, files in os.walk(root, topdown=False):
        for name in [] if data_synced else files:
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


def verify_tree(root, files, check=None, verified=None):
    """Verify the exact serving tree, refusing links and unlisted files."""
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("version root is not a directory")
    expected = {}
    for item in files:
        name = item["path"]
        path = P(name)
        if (
            not isinstance(name, str)
            or path.is_absolute()
            or not path.parts
            or any(part in (".", "..") or part.startswith(".") for part in path.parts)
            or path.as_posix() != name
            or name in expected
            or type(item["size"]) is not int
            or item["size"] < 0
            or not re.fullmatch(r"[0-9a-f]{64}", item["sha256"])
        ):
            raise ValueError("invalid manifest entry")
        expected[name] = item
    actual = set()
    stack = [(root, "")]
    while stack:
        directory, prefix = stack.pop()
        if check is not None:
            check()
        with os.scandir(directory) as entries:
            for entry in entries:
                info = entry.stat(follow_symlinks=False)
                relative = prefix + entry.name
                if stat.S_ISDIR(info.st_mode):
                    stack.append((P(entry.path), relative + "/"))
                    continue
                if relative not in expected or not stat.S_ISREG(info.st_mode):
                    raise ValueError("unexpected version entry")
                item = expected[relative]
                if info.st_size != item["size"]:
                    raise ValueError("version size mismatch")
                if verified is not None:
                    from . import immutable

                    if not immutable.matches_stat(info, item, verified.get(relative)):
                        raise ValueError("verified version identity changed")
                else:
                    digest = hashlib.sha256()
                    with open(entry.path, "rb") as stream:
                        for block in iter(lambda: stream.read(1048576), b""):
                            if check is not None:
                                check()
                            digest.update(block)
                    if digest.hexdigest() != item["sha256"]:
                        raise ValueError("version bytes mismatch")
                actual.add(relative)
    if actual != set(expected):
        raise ValueError("missing version file")


def image_target(manifest):
    sha = manifest["sha"]
    if not SHA.fullmatch(sha):
        raise ValueError("invalid image version")
    return (
        sha + "/images"
        if manifest.get("image_layout") == "snapshot-v1"
        else "images-" + sha
    )


def image_files(files):
    images = [
        dict(item, path=item["path"][7:])
        for item in files
        if item["path"].startswith("images/")
    ]
    if not images:
        raise ValueError("missing images")
    return images


def activate_images(base, target, files, verified=None):
    """Verify before and after switching; restore the exact prior link on error."""
    if verified is None:
        verify_tree(base / "releases" / target, files)
    else:
        verify_tree(base / "releases" / target, files, verified=verified)
    current = base / "current"
    previous = os.readlink(current) if current.is_symlink() else None
    if current.exists() and previous is None:
        raise ValueError("current is not a symlink")
    try:
        switch(base, target)
        if os.readlink(current) != "releases/" + target:
            raise ValueError("activation target changed")
        # Resolve only the deliberate current link; tree contents must be regular.
        if verified is None:
            verify_tree(current.resolve(strict=True), files)
        else:
            verify_tree(current.resolve(strict=True), files, verified=verified)
    except Exception:
        if previous is None:
            current.unlink(missing_ok=True)
        else:
            tmp = base / (".restore-" + uuid.uuid4().hex)
            os.symlink(previous, tmp)
            os.replace(tmp, current)
        fd = os.open(base, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
        raise


def rollback_images(c, key, sha):
    repo(c, key)
    if not SHA.fullmatch(sha):
        raise ValueError("invalid SHA")
    with lock(c):
        rollback_images_locked(c, key, sha)


def rollback_images_locked(c, key, sha):
    """Caller holds Worker lock; compensate current and published on failure."""
    from .object_retention import restoration_record

    restoration_record(c, key, sha)
    state = P(c["state_root"])
    from .retention import read_json

    manifest = read_json(state / "manifests" / key / (sha + ".json"))
    if manifest.get("key") != key or manifest.get("sha") != sha:
        raise ValueError("manifest identity mismatch")
    base = P(c["images_root"]) / "repos" / key
    verify_tree(base / "releases" / sha, manifest["files"])
    from .retention import remember_previous

    if (base / "current").exists() and not (base / "current").is_symlink():
        raise ValueError("current is not a symlink")
    previous = (
        os.readlink(base / "current") if (base / "current").is_symlink() else None
    )
    published = state / "published" / key
    had_published = published.exists() or published.is_symlink()
    saved = read_json(published) if had_published else None
    remember_previous(c, key, sha)
    try:
        activate_images(base, image_target(manifest), image_files(manifest["files"]))
        atomic(published, manifest)
    except Exception:
        if previous is None:
            (base / "current").unlink(missing_ok=True)
            from .retention import sync_directory

            sync_directory(base)
        elif (
            not (base / "current").is_symlink()
            or os.readlink(base / "current") != previous
        ):
            if not re.fullmatch(
                r"releases/(?:images-[0-9a-f]{40}|[0-9a-f]{40}/images)", previous
            ):
                raise ValueError("invalid previous activation reference")
            switch(base, previous.removeprefix("releases/"))
        if not had_published:
            published.unlink(missing_ok=True)
            if published.parent.exists():
                from .retention import sync_directory

                sync_directory(published.parent)
        else:
            atomic(published, saved)
        raise


def publish(c, j):
    """Replay a publication, checking existing immutable bytes before activation.

    A failure may leave uploaded objects or staging data. Never compensate by
    deleting them: a preceding attempt may already have made them readable.
    """
    from .resources import fetch
    from .publication import upload, link_or_copy, policy as publication_policy

    publication_policy(c)
    started = last_stage = time.monotonic()
    j["timings"] = {}

    def stage(name):
        nonlocal last_stage
        now = time.monotonic()
        j["timings"][name] = round(now - last_stage, 6)
        last_stage = now

    r = repo(c, j["key"])
    state = P(c["state_root"])
    g = state / "git" / r["key"]
    g.parent.mkdir(parents=True, exist_ok=True)
    if not g.exists():
        subprocess.run(
            ["git", "init", "--bare", str(g)], check=True, stdout=subprocess.DEVNULL
        )
    fetch(c, r, g)
    head = git(g, "rev-parse", "refs/heads/source", c=c).decode().strip()
    if j["kind"] == "auto" and head != j["sha"]:
        j["status"] = "superseded"
        return
    git(g, "merge-base", "--is-ancestor", j["sha"], head, c=c)
    stage("fetch")
    if shutil.disk_usage(c["images_root"]).free < r.get("max_bytes", 1073741824) * 2:
        raise ValueError("insufficient staging capacity")
    base = P(c["images_root"]) / "repos" / r["key"]
    (base / "releases").mkdir(parents=True, exist_ok=True)
    staging = P(tempfile.mkdtemp(prefix=".staging-", dir=base))
    try:
        from . import incremental, object_refs, immutable

        reuse = incremental.context(c, r)
        reuse["reference_mode"] = object_refs.enabled(c)
        files = export(c, r, g, j["sha"], staging, reuse=reuse)
        images = image_files(files)  # Validate before the first remote write.
        stage("export")
        s = s3client(c, bounded=True)
        prefix = f"repositories/{r['key']}/releases/{j['sha']}/"
        etags = upload(
            c,
            s,
            prefix,
            staging,
            files,
            reuse=reuse["reused"],
            stats=reuse.setdefault("object_stats", {}),
        )
        stage("objects")
        manifest = dict(key=r["key"], sha=j["sha"], files=files, config=c["revision"])
        if reuse["reference_mode"]:
            reuse["object_sources"] = {
                name: dict(key=value["key"], content_type=value["object_content_type"])
                for name, value in reuse["reused"].items()
            }
            manifest.update(
                object_layout="references-v1",
                object_sources=reuse["object_sources"],
                image_layout="snapshot-v1",
            )
            object_refs.validate(manifest)
        from .object_retention import restoration_record

        restoration_record(c, r["key"], j["sha"], restored=True)
        atomic(state / "manifests" / r["key"] / (j["sha"] + ".json"), manifest)
        final = base / "releases" / j["sha"]
        if final.exists():
            verify_tree(final, files)
            if reuse["reference_mode"]:
                reuse["verified"] = {
                    item["path"]: immutable.record(final / item["path"], item)
                    for item in files
                }
        else:
            sync_tree(staging, data_synced=reuse["reference_mode"])
            os.rename(staging, final)
            from .retention import sync_directory

            sync_directory(final.parent)
        # Keep the complete public snapshot and a separate image-serving subset.
        image_final = base / "releases" / image_target(manifest)
        if not (final / "images").is_dir():
            raise ValueError("missing images directory")
        if not image_final.exists():
            image_tmp = P(tempfile.mkdtemp(prefix=".images-", dir=base))
            try:
                shutil.copytree(
                    final / "images",
                    image_tmp,
                    dirs_exist_ok=True,
                    copy_function=(
                        lambda src, dst: immutable.link(
                            P(src),
                            P(dst),
                            reuse["verified"][P(src).relative_to(final).as_posix()],
                        )
                    )
                    if reuse["reference_mode"]
                    else link_or_copy,
                )
                sync_tree(image_tmp, data_synced=reuse["reference_mode"])
                os.rename(image_tmp, image_final)
            finally:
                shutil.rmtree(image_tmp, ignore_errors=True)
        stage("snapshot")
        # Recheck before activation. A Push can still race after this check;
        # Git head and the filesystem link do not share a commit boundary.
        if reuse["reference_mode"] and j["kind"] == "auto":
            from .resources import remote_head

            head = remote_head(c, r, g)
        else:
            fetch(c, r, g)
            head = git(g, "rev-parse", "refs/heads/source", c=c).decode().strip()
        if j["kind"] == "auto" and head != j["sha"]:
            j["status"] = "superseded"
            return
        previous = (
            os.readlink(base / "current") if (base / "current").is_symlink() else None
        )
        j.update(status="images_prepared", previous=previous)
        atomic(state / "jobs" / (j["id"] + ".json"), j)
        from .retention import remember_previous

        remember_previous(c, r["key"], j["sha"])
        if reuse["reference_mode"]:
            image_proofs = {
                name[7:]: value
                for name, value in reuse["verified"].items()
                if name.startswith("images/")
            }
            # activate_images performs complete identity checks before and
            # after switching. A third partial scan adds no safety barrier.
            activate_images(base, image_target(manifest), images, verified=image_proofs)
        else:
            activate_images(base, "images-" + j["sha"], images)
        j["status"] = "published"
        j["finished"] = time.time()
        stage("activation")
        j["timings"]["total"] = round(time.monotonic() - started, 6)
        atomic(state / "published" / r["key"], manifest)
        j["incremental"] = dict(
            reused_files=reuse["reused_files"],
            changed_files=len(files) - reuse["reused_files"],
            object_transfers=reuse["object_stats"],
        )
        # A failed optimization receipt must not turn a completed activation into failure.
        try:
            incremental.save(c, r["key"], j["sha"], files, reuse, etags)
        except (OSError, ValueError, KeyError, TypeError):
            j["incremental"]["seal"] = "unavailable"
        stage("evidence")
        j["finished"] = time.time()
        j["timings"]["total"] = round(time.monotonic() - started, 6)
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def read_job(path):
    """Bound reads before parsing; broken records must not block valid jobs."""
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("nonregular job")
        raw = stream.read(65537)
    if len(raw) > 65536:
        raise ValueError("oversized job")
    j = json.loads(raw)
    if not isinstance(j, dict):
        raise ValueError("invalid job object")
    if (
        not isinstance(j.get("key"), str)
        or not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", j["key"])
        or not isinstance(j.get("sha"), str)
        or not SHA.fullmatch(j["sha"])
        or j["sha"] == "0" * 40
        or j.get("kind") not in ("auto", "manual")
        or j.get("status")
        not in (
            "pending",
            "running",
            "images_prepared",
            "published",
            "failed",
            "superseded",
        )
        or type(j.get("created")) not in (int, float)
        or j["created"] > 1e12
        or not math.isfinite(j["created"])
        or j["created"] < 0
        or not isinstance(j.get("delivery"), str)
        or len(j["delivery"]) > 128
        or not isinstance(j.get("config"), str)
        or not j["config"]
    ):
        raise ValueError("invalid job structure")
    identity = hashlib.sha256(
        (j["key"] + ":" + j["sha"] + ":" + j["kind"]).encode()
    ).hexdigest()
    if j.get("id") != identity or path.name != identity + ".json":
        raise ValueError("job identity mismatch")
    return j


def queue_notice(event, error):
    # No task payload, paths, remote URL or exception text enters container logs.
    print(
        json.dumps(dict(event=event, error=type(error).__name__)),
        file=sys.stderr,
        flush=True,
    )


def quarantine_job(c, path, error):
    with lock(c, "queue"):
        _quarantine_job(c, path, error)


def _quarantine_job(c, path, error):
    folder = P(c["state_root"]) / "quarantine" / uuid.uuid4().hex
    folder.mkdir(parents=True, mode=0o700)
    os.replace(path, folder / "record")
    atomic(
        folder / "reason.json",
        dict(
            original_name=path.name,
            quarantined_at=time.time(),
            error=type(error).__name__,
        ),
    )
    fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)
    queue_notice("job-quarantined", error)


def work_once(c, heartbeat=None):
    """Isolate invalid records; keep failure/retry semantics for valid jobs."""
    with contextlib.ExitStack() as stack:
        try:
            stack.enter_context(lock(c, blocking=False))
        except BlockingIOError:
            return  # A live maintenance holder must not stall the loop heartbeat.
        jobs = P(c["state_root"]) / "jobs"
        jobs.mkdir(parents=True, exist_ok=True)
        ready = []
        for path in jobs.glob("*.json"):
            try:
                j = read_job(path)
            except FileNotFoundError:
                continue
            except (
                ValueError,
                TypeError,
                KeyError,
                UnicodeError,
                OSError,
                RecursionError,
            ) as error:
                try:
                    quarantine_job(c, path, error)
                except OSError as failure:
                    queue_notice("job-quarantine-failed", failure)
                continue
            if j["status"] in ("pending", "running", "images_prepared"):
                ready.append((path, j))
        for path, j in sorted(
            ready, key=lambda item: (item[1]["created"], item[0].name)
        ):
            try:
                j["status"] = "running"
                atomic(path, j)
                if heartbeat is not None:
                    heartbeat.update(
                        job_started_at=time.time(), progress_at=time.time()
                    )
                try:
                    publish(c, j)
                except Exception as error:
                    j.update(status="failed", error=type(error).__name__)
                    from .resources import ResourceLimit

                    if isinstance(error, ResourceLimit):
                        j["resource_limit"] = str(error)
                        from .resource_events import record

                        with lock(c, "queue"):
                            record(c, error, j["key"])
                atomic(path, j)
            except OSError as error:
                queue_notice("job-state-write-failed", error)
            finally:
                if heartbeat is not None:
                    heartbeat.update(job_started_at=None, progress_at=time.time())
        if not ready:
            from .incremental import audit_idle

            audit_idle(c)


def render(c, directory):
    """Write a candidate Nginx file; the caller owns validation and reload."""
    from .svg import CSP

    directory = P(directory)
    directory.mkdir(parents=True, exist_ok=True)
    lines = [
        "include /etc/nginx/mime.types;",
        "default_type application/octet-stream;",
        "server { listen 8080; server_tokens off; autoindex off; open_file_cache off;",
        "add_header X-Content-Type-Options nosniff always;",
        f'add_header Content-Security-Policy "{CSP}" always;',
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
        except Exception as error:
            from .resources import ResourceLimit

            self.send_response(503)
            self.send_header("Retry-After", "60")
            self.end_headers()
            if isinstance(error, ResourceLimit):
                self.wfile.write(
                    json.dumps(
                        {"error": "resource_limit", "limit": str(error)}
                    ).encode()
                )


def active_image_state(config, key, sha):
    """Read current separately from a job's immutable historical result."""
    from .retention import current_sha, read_json

    try:
        current = current_sha(P(config["images_root"]) / "repos" / key)
        published = read_json(P(config["state_root"]) / "published" / key)
        if published.get("sha") != current:
            return "unknown"
        return "current" if current == sha else "other-version"
    except FileNotFoundError:
        return "unavailable"
    except (OSError, ValueError, KeyError, TypeError):
        return "unknown"


def main():
    from .cli import parse

    a, o = parse()
    try:
        execute(a, o)
    except KeyboardInterrupt:
        print(
            json.dumps(
                dict(
                    status="interrupted",
                    command=o.command,
                    hint="先查询任务或核对报告，确认状态后再重试。",
                ),
                ensure_ascii=False,
            ),
            file=sys.stderr,
        )
        raise SystemExit(130) from None
    except Exception as error:
        from .resources import ResourceLimit

        print(
            json.dumps(
                dict(
                    status="failed",
                    resource_limit=str(error)
                    if isinstance(error, ResourceLimit)
                    else None,
                    command=o.command,
                    error=type(error).__name__,
                    hint="先核对仓库 key/任务 ID，并在宿主机运行 scripts/open-assets doctor；原始异常不输出。",
                ),
                ensure_ascii=False,
            ),
            file=sys.stderr,
        )
        raise SystemExit(1) from None


def execute(a, o):
    try:
        c = load()
    except Exception as error:
        if o.command in (
            "restore-objects",
            "check-restore",
            "close-restore",
        ) or o.command.startswith("restore-audit-"):
            print(
                json.dumps(dict(status="failed", error=type(error).__name__)),
                file=sys.stderr,
            )
            raise SystemExit(1) from None
        raise
    if o.command == "validate":
        print("configuration valid")
    elif o.command == "render":
        render(c, o.args[0])
    elif o.command == "serve":
        http.server.ThreadingHTTPServer(("0.0.0.0", 9000), Handler).serve_forever()
    elif o.command == "worker":
        # Create the shared lock even before the first event is admitted.
        with lock(c, "queue"):
            pass
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
        from .queue_wakeup import QueueWakeup

        directory = P(c["state_root"]) / "jobs"
        directory.mkdir(parents=True, exist_ok=True)
        wakeup = QueueWakeup(directory)
        try:
            delay = 2
            while True:
                wakeup.drain()
                heartbeat["progress_at"] = time.time()
                try:
                    work_once(load(), heartbeat)
                    delay = 2
                except OSError as error:
                    queue_notice("queue-io-failed", error)
                    delay = min(delay * 2, 60)
                    time.sleep(delay)
                    continue
                wakeup.wait(delay)
        finally:
            wakeup.close()
            stop.set()
    elif o.command == "compact-objects":
        from .object_compact import run

        print(json.dumps(run(c, s3client(c, bounded=True), apply=o.apply)))
    elif o.command == "materialize-image-views":
        from .local_dedup import legacy_views

        print(json.dumps(legacy_views(c, apply=o.apply)))
    elif o.command == "deduplicate-local":
        from .local_dedup import run

        print(json.dumps(run(c, apply=o.apply)))
    elif o.command == "once":
        work_once(c)
    elif o.command == "publish":
        print(enqueue(c, *o.args, "manual", kind="manual"))
    elif o.command == "restore-objects":
        if len(o.args) != 2:
            a.error("restore-objects requires a repository key and full commit SHA")
        from .object_restore import run as restore_objects

        try:
            report = restore_objects(
                c, *o.args, with_local=o.with_local, rollback=o.rollback
            )
        except (Exception, KeyboardInterrupt) as error:
            print(
                json.dumps(dict(status="failed", error=type(error).__name__)),
                file=sys.stderr,
            )
            raise SystemExit(
                130 if isinstance(error, KeyboardInterrupt) else 1
            ) from None
        print(json.dumps(report))
        if report["status"] == "busy":
            raise SystemExit(2)
    elif o.command in ("check-restore", "close-restore"):
        if len(o.args) != 2:
            a.error(o.command + " requires a repository key and full commit SHA")
        try:
            if o.command == "close-restore":
                from .restore_close import run as close_restore

                report = close_restore(c, *o.args, reason=o.reason)
            else:
                from .restore_check import run as check_restore

                report = check_restore(c, *o.args)
        except (Exception, KeyboardInterrupt) as error:
            print(
                json.dumps(dict(status="failed", error=type(error).__name__)),
                file=sys.stderr,
            )
            raise SystemExit(1) from None
        print(json.dumps(report))
        if report["status"] == "busy":
            raise SystemExit(2)
        if report.get("diagnosis") != "verified" and report["status"] != "closed":
            raise SystemExit(1)
    elif o.command.startswith("restore-audit-"):
        if not 1 <= len(o.args) <= 2:
            a.error(o.command + " requires a repository key and optional full SHA")
        from .restore_audit import run as audit_run

        options = {
            k: v
            for k, v in dict(
                keep_last=o.keep_last,
                min_age_days=o.min_age_days,
                max_records=o.max_records,
                limit=o.limit,
                offset=o.offset,
            ).items()
            if v is not None
        }
        try:
            report = audit_run(
                c,
                *o.args,
                action=o.command.removeprefix("restore-audit-"),
                apply=o.apply,
                **options,
            )
        except (Exception, KeyboardInterrupt) as error:
            print(
                json.dumps(dict(status="failed", error=type(error).__name__)),
                file=sys.stderr,
            )
            raise SystemExit(1) from None
        print(json.dumps(report))
        if report["status"] != "complete":
            raise SystemExit(2 if report["status"] == "busy" else 1)
    elif o.command == "retry":
        if not re.fullmatch("[0-9a-f]{64}", o.args[0]):
            raise ValueError("invalid job ID")
        with lock(c):
            p = P(c["state_root"]) / "jobs" / (o.args[0] + ".json")
            j = json.loads(p.read_text())
            if j["status"] != "failed":
                raise ValueError("only failed jobs may retry")
            j["status"] = "pending"
            j.pop("resource_limit", None)
            atomic(p, j)
    elif o.command == "rollback-images":
        rollback_images(c, *o.args)
    else:
        if o.args not in ([], ["all"], ["archived"]):
            raise ValueError("status accepts all or archived")
        active = (P(c["state_root"]) / "jobs").glob("*.json")
        archived = (P(c["state_root"]) / "job-archive").glob("*/*.json")
        paths = archived if o.args == ["archived"] else active
        if o.args == ["all"]:
            paths = itertools.chain(active, archived)
        for p in paths:
            try:
                j = read_job(p)
            except (
                OSError,
                ValueError,
                TypeError,
                KeyError,
                UnicodeError,
                RecursionError,
            ) as error:
                queue_notice("invalid-job-record", error)
                continue
            if o.status_key and j["key"] != o.status_key:
                continue
            if o.status_sha and j["sha"] != o.status_sha:
                continue
            values = [j["id"], j["key"], j["sha"], j["status"]]
            if j.get("resource_limit") and j["status"] == "failed":
                values.append(j["resource_limit"])
            values.append("images=" + active_image_state(c, j["key"], j["sha"]))
            print(*values)


if __name__ == "__main__":
    main()
