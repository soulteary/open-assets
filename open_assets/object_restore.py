"""Restore a known release directly from Git without replaying or altering jobs."""

import contextlib
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time

from . import retention

CACHE_CONTROL = "public, max-age=31536000, immutable"
MAX_FILES = 10000
MAX_REQUESTS = 50000


class Budget:
    def __init__(self):
        limit = int(os.environ.get("WORKER_MAX_JOB_SECONDS", "1800"))
        if not 60 <= limit <= 86400:
            raise ValueError("invalid restoration time limit")
        self.deadline = time.monotonic() + limit
        self.requests = 0
        self.next_request = 0

    def check(self):
        retention.check_deadline(self.deadline)

    def call(self, s3, method, **kwargs):
        self.check()
        if self.requests >= MAX_REQUESTS:
            raise ValueError("restoration request limit")
        time.sleep(max(0, self.next_request - time.monotonic()))
        self.check()
        self.next_request = time.monotonic() + 0.05
        self.requests += 1
        return getattr(s3, method)(**kwargs)


def report_path(config, key, sha):
    return Path(config["state_root"]) / "object-restores" / key / (sha + ".json")


def directories(*paths):
    for path in paths:
        if path.exists() or path.is_symlink():
            retention.real_directory(path)
        else:
            path.mkdir()


def canonical_files(manifest, key, sha):
    if (
        not isinstance(manifest, dict)
        or manifest.get("key") != key
        or manifest.get("sha") != sha
    ):
        raise ValueError("restoration manifest identity mismatch")
    files = manifest.get("files")
    if not isinstance(files, list) or not 0 < len(files) <= MAX_FILES:
        raise ValueError("restoration manifest file limit")
    result = {}
    for item in files:
        name = item["path"]
        path = Path(name)
        mime = item.get("content_type")
        if (
            not isinstance(name, str)
            or path.is_absolute()
            or not path.parts
            or path.as_posix() != name
            or any(part.startswith(".") for part in path.parts)
            or name in result
            or type(item["size"]) is not int
            or item["size"] < 0
            or not isinstance(item["sha256"], str)
            or not re.fullmatch(r"[a-f0-9]{64}", item["sha256"])
            or not isinstance(mime, str)
            or not 1 <= len(mime) <= 256
            or any(ord(c) < 32 or ord(c) > 126 for c in mime)
        ):
            raise ValueError("invalid restoration manifest file")
        result[name] = {k: item[k] for k in ("path", "size", "sha256", "content_type")}
    return result


def listing(config, prefix, expected, s3, budget, subset=False):
    found = {}
    token = None
    tokens = set()
    while True:
        args = dict(Bucket=config["bucket"], Prefix=prefix, MaxKeys=1000)
        if token:
            args["ContinuationToken"] = token
        page = budget.call(s3, "list_objects_v2", **args)
        for obj in page.get("Contents", []):
            key = obj["Key"]
            if key not in expected and subset:
                continue
            if (
                key not in expected
                or key in found
                or obj.get("Size") != expected[key]["size"]
            ):
                raise ValueError("unexpected restoration object or size")
            found[key] = obj
        if page.get("IsTruncated") is False:
            return found
        token = page.get("NextContinuationToken")
        if (
            page.get("IsTruncated") is not True
            or not isinstance(token, str)
            or not 0 < len(token) <= 16384
            or token in tokens
        ):
            raise ValueError("invalid restoration pagination")
        tokens.add(token)


def expected_objects(manifest, key, sha):
    from . import object_refs

    files = canonical_files(manifest, key, sha)
    sources = object_refs.validate(manifest)
    expected = {}
    for name, item in files.items():
        value = dict(item, content_type=sources[name]["content_type"])
        object_key = sources[name]["key"]
        previous = expected.get(object_key)
        if previous is not None and (
            previous["size"],
            previous["sha256"],
            previous["content_type"],
        ) != (value["size"], value["sha256"], value["content_type"]):
            raise ValueError("conflicting shared restoration object")
        expected[object_key] = value
    return expected


def listing_manifest(config, manifest, expected, s3, budget):
    prefix = f"repositories/{manifest['key']}/releases/{manifest['sha']}/"
    found = {}
    prefixes = {"/".join(name.split("/")[:4]) + "/" for name in expected}
    for owner_prefix in sorted(prefixes):
        selected = {k: v for k, v in expected.items() if k.startswith(owner_prefix)}
        found.update(
            listing(
                config,
                owner_prefix,
                selected,
                s3,
                budget,
                subset=owner_prefix != prefix,
            )
        )
    return found


def get_verified(config, key, item, s3, budget, missing=False):
    try:
        response = budget.call(s3, "get_object", Bucket=config["bucket"], Key=key)
    except s3.exceptions.NoSuchKey:
        if missing:
            return None
        raise ValueError("restoration object disappeared") from None
    body = response["Body"]
    digest = hashlib.sha256()
    size = 0
    try:
        while True:
            budget.check()
            block = body.read(min(1048576, item["size"] - size + 1))
            if not block:
                break
            size += len(block)
            if size > item["size"]:
                raise ValueError("restoration object exceeds expected size")
            digest.update(block)
    finally:
        body.close()
    if size != item["size"] or digest.hexdigest() != item["sha256"]:
        raise ValueError("immutable restoration object conflict")
    return response


def metadata_matches(response, item):
    return (
        response is not None
        and response.get("Metadata", {}).get("sha256") == item["sha256"]
        and response.get("ContentType") == item["content_type"]
        and response.get("CacheControl") == CACHE_CONTROL
    )


def prepare_local(base, sha, staging, files, budget, direct=False):
    """Validate every existing copy before network writes; never replace conflicts."""
    from . import __main__ as core

    releases = base / "releases"
    directories(releases)
    full = releases / sha
    image = full / "images" if direct else releases / ("images-" + sha)
    image_files = core.image_files(files)
    for path, expected in ((full, files), (image, image_files)):
        if path.exists() or path.is_symlink():
            core.verify_tree(path, expected, check=budget.check)
    if direct or image.exists():
        return None
    temporary = Path(tempfile.mkdtemp(prefix=".restore-images-", dir=base))
    try:
        shutil.copytree(staging / "images", temporary, dirs_exist_ok=True)
        core.verify_tree(temporary, image_files, check=budget.check)
        core.sync_tree(temporary)
        return temporary
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise


def install_local(
    config, key, sha, staging, image_staging, files, report, budget, direct=False
):
    from . import __main__ as core

    releases = Path(config["images_root"]) / "repos" / key / "releases"
    entries = [
        (staging, releases / sha, files, "local_full_rebuilt"),
    ]
    if not direct:
        entries.append(
            (
                image_staging,
                releases / ("images-" + sha),
                core.image_files(files),
                "local_images_rebuilt",
            )
        )
    for source, target, expected, field in entries:
        budget.check()
        if target.exists() or target.is_symlink():
            core.verify_tree(target, expected, check=budget.check)
            continue
        if source is None:
            raise ValueError("missing prepared local restoration copy")
        if not report["local_partial_possible"]:
            report["local_partial_possible"] = True
            core.atomic(report_path(config, key, sha), report)
        core.sync_tree(source)
        os.rename(source, target)
        report[field] = True
        retention.sync_directory(releases)
        retention.sync_directory(source.parent)
        core.verify_tree(target, expected, check=budget.check)
    # A successful pair is verified again after both renames, not inferred from flags.
    core.verify_tree(releases / sha, files, check=budget.check)
    core.verify_tree(
        releases / (sha + "/images" if direct else "images-" + sha),
        core.image_files(files),
        check=budget.check,
    )
    report.update(local_verified=True, local_partial_possible=False)


def restore_locked(config, repository, sha, report, s3, budget):
    from . import __main__ as core
    from .object_retention import (
        manifest as retention_manifest,
        receipt,
        restoration_record,
    )

    key = repository["key"]
    state = Path(config["state_root"])
    images = Path(config["images_root"])
    directories(state, state / "manifests", state / "manifests" / key)
    manifest_path = state / "manifests" / key / (sha + ".json")
    original = retention.read_json(manifest_path)
    files = canonical_files(original, key, sha)
    _, digest = retention_manifest(config, key, sha)
    # A partial-deletion receipt must still refer to this manifest and bucket.
    receipt(config, key, sha, digest)
    from .object_retention import receipt_path

    marker = receipt_path(config, key, sha)
    if marker.exists() or marker.is_symlink():
        value = retention.read_json(marker)
        if (
            value.get("schema_version") != 1
            or value.get("key") != key
            or value.get("sha") != sha
            or value.get("status") not in ("deleting", "complete", "restored")
        ):
            raise ValueError("invalid restoration receipt")
    directories(state / "git", state / "git" / key)
    git_dir = state / "git" / key
    if not (git_dir / "HEAD").exists():
        subprocess.run(
            ["git", "init", "--bare", str(git_dir)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=30,
        )
    report["phase"] = "git"
    budget.check()
    from .resources import fetch

    try:
        fetch(config, repository, git_dir, check=budget.check)
    except Exception as error:
        from .resource_events import record

        with core.lock(config, "queue"):
            record(config, error, key)
        raise
    budget.check()
    if core.git(git_dir, "cat-file", "-t", sha, c=config).strip() != b"commit":
        raise ValueError("restoration target must be a commit")
    core.git(git_dir, "merge-base", "--is-ancestor", sha, "refs/heads/source", c=config)
    quota = repository.get("max_bytes", 1073741824)
    if (
        type(quota) is not int
        or quota <= 0
        or shutil.disk_usage(images).free < quota * (3 if report["with_local"] else 2)
    ):
        raise ValueError("insufficient restoration staging capacity")
    directories(images, images / "repos", images / "repos" / key)
    staging = Path(tempfile.mkdtemp(prefix=".restore-", dir=images / "repos" / key))
    image_staging = None
    try:
        report["phase"] = "export"
        rebuilt = core.export(
            config, repository, git_dir, sha, staging, check=budget.check
        )
        budget.check()
        actual = {item["path"]: (item["size"], item["sha256"]) for item in rebuilt}
        if actual != {
            name: (item["size"], item["sha256"]) for name, item in files.items()
        }:
            raise ValueError("Git export differs from original restoration manifest")
        core.verify_tree(staging, list(files.values()), check=budget.check)
        if report["with_local"]:
            report["phase"] = "local-preflight"
            image_staging = prepare_local(
                images / "repos" / key,
                sha,
                staging,
                list(files.values()),
                budget,
                direct=original.get("image_layout") == "snapshot-v1",
            )
        if report["rollback_requested"]:
            # Refuse to switch on top of inconsistent current/published metadata.
            retention.protected_references(config, key, [], [], budget.deadline)
        expected = expected_objects(original, key, sha)

        def list_expected():
            return listing_manifest(config, original, expected, s3, budget)

        report.update(
            phase="preflight",
            files=len(files),
            logical_bytes=sum(item["size"] for item in files.values()),
        )
        found = list_expected()
        repairs = set()
        # All pre-existing bytes must pass before the first write.
        for name in found:
            if not metadata_matches(
                get_verified(config, name, expected[name], s3, budget), expected[name]
            ):
                repairs.add(name)
        report["phase"] = "upload"
        for name, item in expected.items():
            if name in found and name not in repairs:
                continue
            current = get_verified(config, name, item, s3, budget, missing=True)
            if metadata_matches(current, item):
                continue
            metadata = dict(current.get("Metadata", {})) if current else {}
            metadata["sha256"] = item["sha256"]
            if not report["partial_possible"]:
                # Persist write intent before the first PUT, including SIGKILL cases.
                report["partial_possible"] = True
                core.atomic(report_path(config, key, sha), report)
            with (staging / item["path"]).open("rb") as stream:
                budget.call(
                    s3,
                    "put_object",
                    Bucket=config["bucket"],
                    Key=name,
                    Body=stream,
                    Metadata=metadata,
                    ContentType=item["content_type"],
                    CacheControl=CACHE_CONTROL,
                )
            report["uploaded_objects" if current is None else "repaired_objects"] += 1
        report["phase"] = "verify"
        if set(list_expected()) != set(expected):
            raise ValueError("incomplete restored object set")
        for name, item in expected.items():
            if not metadata_matches(get_verified(config, name, item, s3, budget), item):
                raise ValueError("restored object metadata mismatch")
            report["verified_objects"] += 1
        if retention.read_json(manifest_path) != original:
            raise ValueError("restoration manifest changed")
        if report["with_local"]:
            report["phase"] = "local-install"
            install_local(
                config,
                key,
                sha,
                staging,
                image_staging,
                list(files.values()),
                report,
                budget,
                direct=original.get("image_layout") == "snapshot-v1",
            )
        # Preserve original manifest bytes/MIME types and grant a new retention window.
        os.utime(manifest_path, None)
        restoration_record(config, key, sha, restored=True)
        cache = state / "incremental" / (key + ".json")
        if cache.exists() or cache.is_symlink():
            core.atomic(cache, dict(version=0, reason="explicit-object-restoration"))
        if report["rollback_requested"]:
            budget.check()
            report.update(phase="activate", activation_uncertain=True)
            core.atomic(report_path(config, key, sha), report)
            core.rollback_images_locked(config, key, sha)
            report.update(activated=True, activation_uncertain=False)
        report.update(phase="complete", partial_possible=False, manifest_digest=digest)
    finally:
        shutil.rmtree(staging, ignore_errors=True)
        if image_staging is not None:
            shutil.rmtree(image_staging, ignore_errors=True)


def run(config, key, sha, s3=None, *, with_local=False, rollback=False):
    from . import __main__ as core
    from .object_retention import bucket_scope

    if (
        not isinstance(key, str)
        or not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", key)
        or not retention.valid_sha(sha)
    ):
        raise ValueError("invalid restoration identity")
    repository = core.repo(config, key)
    if (
        type(with_local) is not bool
        or type(rollback) is not bool
        or (rollback and not with_local)
    ):
        raise ValueError("rollback requires local restoration")
    budget = Budget()
    with contextlib.ExitStack() as stack:
        try:
            stack.enter_context(core.lock(config, blocking=False))
        except BlockingIOError:
            return dict(status="busy", key=key, sha=sha)
        report = dict(
            schema_version=1,
            key=key,
            sha=sha,
            status="running",
            phase="validate",
            started_at=time.time(),
            scope_digest=bucket_scope(config),
            uploaded_objects=0,
            repaired_objects=0,
            verified_objects=0,
            partial_possible=False,
            with_local=with_local,
            rollback_requested=rollback,
            local_full_rebuilt=False,
            local_images_rebuilt=False,
            local_verified=False,
            local_partial_possible=False,
            activated=False,
            activation_uncertain=False,
        )
        path = report_path(config, key, sha)
        directories(path.parent.parent, path.parent)
        core.atomic(path, report)
        try:
            restore_locked(
                config,
                repository,
                sha,
                report,
                s3 or core.s3client(config, bounded=True),
                budget,
            )
        except (Exception, KeyboardInterrupt) as error:
            report.update(
                status="failed", error=type(error).__name__, finished_at=time.time()
            )
            core.atomic(path, report)
            raise
        report.update(status="complete", finished_at=time.time())
        core.atomic(path, report)
        return report
