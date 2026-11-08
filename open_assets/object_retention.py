"""Opt-in, manifest-scoped S3 retention. Preview is the default, never a saved plan."""

import argparse
import contextlib
import fcntl
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import sys
import time

from . import retention as local
from .object_info import client, scope

DEFAULTS = dict(
    enabled=False,
    exclusive_writer=False,
    keep_last=10,
    keep_days=90,
    min_age_hours=24,
    pinned_shas=[],
    max_delete_versions=2,
    max_delete_objects=100,
    max_delete_bytes=268435456,
)
MAX_OBJECTS = 50000
MAX_REQUESTS = 500
SECONDS = 45


def policy(config, repository):
    result = dict(DEFAULTS)
    for source in (
        config.get("object_retention", {}),
        repository.get("object_retention", {}),
    ):
        if not isinstance(source, dict) or set(source) - set(DEFAULTS):
            raise ValueError("invalid object retention options")
        result.update(source)
    if (
        type(result["enabled"]) is not bool
        or type(result["exclusive_writer"]) is not bool
    ):
        raise ValueError("invalid object retention enabled")
    for name, low, high in [
        ("keep_last", 1, 10000),
        ("keep_days", 0, 36500),
        ("min_age_hours", 1, 876000),
        ("max_delete_versions", 1, 10),
        ("max_delete_objects", 1, 1000),
        ("max_delete_bytes", 1, 10737418240),
    ]:
        if type(result[name]) is not int or not low <= result[name] <= high:
            raise ValueError("invalid object retention limit: " + name)
    pins = result["pinned_shas"]
    if (
        not isinstance(pins, list)
        or len(pins) > 1000
        or any(not local.valid_sha(s) for s in pins)
    ):
        raise ValueError("invalid object retention pins")
    return result


class Budget:
    def __init__(self, s3):
        self.s3, self.deadline, self.requests, self.objects = (
            s3,
            time.monotonic() + SECONDS,
            0,
            0,
        )
        self.next_request = 0

    def call(self, operation, **kwargs):
        local.check_deadline(self.deadline)
        if self.requests >= MAX_REQUESTS:
            raise ValueError("object retention request budget exhausted")
        time.sleep(max(0, self.next_request - time.monotonic()))
        local.check_deadline(self.deadline)
        self.next_request = time.monotonic() + 0.1
        self.requests += 1
        return getattr(self.s3, operation)(**kwargs)


def prefix(key, sha=None):
    return f"repositories/{key}/releases/" + (sha + "/" if sha else "")


def listing(config, key, budget, sha=None):
    objects = {}
    token = None
    seen = set()
    while True:
        args = dict(Bucket=config["bucket"], Prefix=prefix(key, sha), MaxKeys=1000)
        if token:
            args["ContinuationToken"] = token
        page = budget.call("list_objects_v2", **args)
        for item in page.get("Contents", []):
            name = item["Key"]
            size = item["Size"]
            etag = item["ETag"]
            modified = item["LastModified"].timestamp()
            if (
                not isinstance(name, str)
                or not name.startswith(args["Prefix"])
                or name in objects
                or type(size) is not int
                or size < 0
                or not isinstance(etag, str)
                or not re.fullmatch(r'"[^"\r\n]+"', etag)
                or not 0 <= modified <= time.time() + 300
            ):
                raise ValueError("invalid object retention listing")
            budget.objects += 1
            if budget.objects > MAX_OBJECTS:
                raise ValueError("object retention listing limit")
            objects[name] = dict(size=size, etag=etag, modified=modified)
        if page.get("IsTruncated") is False:
            return objects
        token = page.get("NextContinuationToken")
        if (
            page.get("IsTruncated") is not True
            or not isinstance(token, str)
            or not token
            or token in seen
        ):
            raise ValueError("invalid object retention pagination")
        seen.add(token)


def manifest(config, key, sha):
    path = Path(config["state_root"]) / "manifests" / key / (sha + ".json")
    for parent in (path.parent.parent, path.parent):
        if parent.exists() or parent.is_symlink():
            local.real_directory(parent)
    if not path.exists() and not path.is_symlink():
        return None, None
    raw = local.read_json(path)
    if (
        raw.get("key") != key
        or raw.get("sha") != sha
        or not isinstance(raw.get("files"), list)
        or not 0 < len(raw["files"]) <= MAX_OBJECTS
    ):
        raise ValueError("invalid object retention manifest")
    result = {}
    for item in raw["files"]:
        name = item["path"]
        p = PurePosixPath(name)
        if (
            not isinstance(name, str)
            or not p.parts
            or p.is_absolute()
            or p.as_posix() != name
            or any(part.startswith(".") for part in p.parts)
            or name in result
            or type(item["size"]) is not int
            or item["size"] < 0
            or not isinstance(item["sha256"], str)
            or not re.fullmatch(r"[0-9a-f]{64}", item["sha256"])
        ):
            raise ValueError("invalid object retention manifest entry")
        result[prefix(key, sha) + name] = dict(size=item["size"], sha256=item["sha256"])
    digest = hashlib.sha256(json.dumps(result, sort_keys=True).encode()).hexdigest()
    return result, digest


def receipt_path(config, key, sha):
    return Path(config["state_root"]) / "object-retention" / key / (sha + ".json")


def receipt(config, key, sha, digest):
    path = receipt_path(config, key, sha)
    for parent in (path.parent.parent, path.parent):
        if parent.exists() or parent.is_symlink():
            local.real_directory(parent)
    if not path.exists() and not path.is_symlink():
        return None
    data = local.read_json(path)
    if data.get("status") in ("complete", "restored"):
        return None
    if (
        data.get("schema_version") != 1
        or data.get("key") != key
        or data.get("sha") != sha
        or data.get("manifest_digest") != digest
        or data.get("bucket_scope") != bucket_scope(config)
        or data.get("status") != "deleting"
        or not isinstance(data.get("objects"), dict)
    ):
        raise ValueError("invalid object retention receipt")
    return data


def bucket_scope(config):
    return hashlib.sha256(
        json.dumps([config["endpoint"], config["bucket"]]).encode()
    ).hexdigest()


def restoration_record(config, key, sha, restored=False):
    """A local image rollback must not activate an object-deleted release."""
    path = receipt_path(config, key, sha)
    if not path.exists() and not path.is_symlink():
        return
    for parent in (path.parent.parent, path.parent):
        local.real_directory(parent)
    value = local.read_json(path)
    if (
        value.get("key") != key
        or value.get("sha") != sha
        or value.get("status") not in ("complete", "deleting", "restored")
    ):
        raise ValueError("invalid object restoration record")
    if restored:
        from .__main__ import atomic

        atomic(
            path,
            dict(
                schema_version=1,
                key=key,
                sha=sha,
                status="restored",
                restored_at=time.time(),
            ),
        )
    elif value["status"] != "restored":
        raise ValueError(
            "version objects were deleted; republish or restore before image rollback"
        )


def plan(config, repository, jobs, budget, now):
    key = repository["key"]
    settings = policy(config, repository)
    pins = set(settings["pinned_shas"]) | set(
        local.policy(config, repository)["pinned_shas"]
    )
    current, protected = local.protected_references(
        config, key, jobs, pins, budget.deadline
    )
    objects = listing(config, key, budget)
    versions = {}
    unknown = 0
    for name, obj in objects.items():
        suffix = name[len(prefix(key)) :]
        sha, separator, _ = suffix.partition("/")
        if not separator or not local.valid_sha(sha):
            unknown += 1
            continue
        version = versions.setdefault(sha, dict(sha=sha, objects={}, newest=0))
        version["objects"][name] = obj
        version["newest"] = max(version["newest"], obj["modified"])
    receipts = Path(config["state_root"]) / "object-retention" / key
    if receipts.exists() or receipts.is_symlink():
        local.real_directory(receipts)
        for index, path in enumerate(receipts.iterdir()):
            if index >= 10000:
                raise ValueError("object retention receipt limit")
            local.check_deadline(budget.deadline)
            if not re.fullmatch(
                r"[0-9a-f]{40}\.json", path.name
            ) or not local.valid_sha(path.stem):
                raise ValueError("unknown object retention receipt")
            saved = local.read_json(path)
            if saved.get("status") == "deleting":
                versions.setdefault(
                    path.stem, dict(sha=path.stem, objects={}, newest=0)
                )
    for sha, version in versions.items():
        files, digest = manifest(config, key, sha)
        if files is None:
            protected.setdefault(sha, []).append("missing-manifest")
            continue
        version.update(files=files, manifest_digest=digest)
        previous = receipt(config, key, sha, digest)
        names = set(version["objects"])
        expected = set(files)
        if previous:
            if set(previous["objects"]) != expected or any(
                previous["objects"].get(n) != obj
                for n, obj in version["objects"].items()
            ):
                raise ValueError(
                    "object retention receipt conflicts with current objects"
                )
        if names != expected and not previous:
            protected.setdefault(sha, []).append("incomplete-or-extra-objects")
        if any(
            n not in files or obj["size"] != files[n]["size"]
            for n, obj in version["objects"].items()
        ):
            protected.setdefault(sha, []).append("manifest-size-mismatch")
        path = Path(config["state_root"]) / "manifests" / key / (sha + ".json")
        version["newest"] = max(version["newest"], path.stat().st_mtime)
        for marker in ("", "images-"):
            directory = (
                Path(config["images_root"])
                / "repos"
                / key
                / "releases"
                / (marker + sha)
            )
            if directory.exists() or directory.is_symlink():
                local.real_directory(directory)
                version["newest"] = max(version["newest"], directory.stat().st_mtime)
    for _, job in jobs:
        if job["key"] == key and job["sha"] in versions:
            versions[job["sha"]]["newest"] = max(
                versions[job["sha"]]["newest"], job["created"], job.get("finished", 0)
            )
    ordered = sorted(
        versions.values(), key=lambda v: (v["newest"], v["sha"]), reverse=True
    )
    for v in ordered[: settings["keep_last"]]:
        protected.setdefault(v["sha"], []).append("keep-last")
    for v in ordered:
        if current is None:
            protected.setdefault(v["sha"], []).append("no-current-reference")
        if now - v["newest"] < max(
            settings["keep_days"] * 86400, settings["min_age_hours"] * 3600
        ):
            protected.setdefault(v["sha"], []).append("recent")
    candidates = []
    count = size = 0
    eligible_versions = eligible_objects = eligible_bytes = skipped_versions = 0
    skip_counts = {}
    for v in reversed(ordered):
        n = len(v["objects"])
        b = sum(o["size"] for o in v["objects"].values())
        if v["sha"] in protected:
            continue
        eligible_versions += 1
        eligible_objects += n
        eligible_bytes += b
        # Select a bounded batch, retaining the whole observed version for the
        # durable receipt. Oversized versions can progress across invocations.
        batch = {}
        batch_bytes = 0
        for name, obj in sorted(v["objects"].items()):
            if count + len(batch) >= settings["max_delete_objects"]:
                break
            if size + batch_bytes + obj["size"] <= settings["max_delete_bytes"]:
                batch[name] = obj
                batch_bytes += obj["size"]
        if len(candidates) >= settings["max_delete_versions"] or (n and not batch):
            skipped_versions += 1
            reason = (
                "versions-limit"
                if len(candidates) >= settings["max_delete_versions"]
                else "objects-limit"
                if count + n > settings["max_delete_objects"]
                else "bytes-limit"
            )
            skip_counts[reason] = skip_counts.get(reason, 0) + 1
            continue
        v["batch"] = batch
        candidates.append(v)
        count += len(batch)
        size += batch_bytes
    return dict(
        key=key,
        enabled=settings["enabled"],
        exclusive_writer=settings["exclusive_writer"],
        protected=protected,
        unknown_objects=unknown,
        candidates=candidates,
        eligible_versions=eligible_versions,
        eligible_objects=eligible_objects,
        eligible_bytes=eligible_bytes,
        skipped_versions=skipped_versions,
        skip_counts=skip_counts,
    )


def remove(config, repository, version, budget, progress=None):
    from .__main__ import atomic

    key = repository["key"]
    sha = version["sha"]
    if budget.call("get_bucket_versioning", Bucket=config["bucket"]).get(
        "Status"
    ) not in (None, ""):
        raise ValueError("bucket versioning changed")
    # Re-list the full version, then HEAD this batch before its first deletion.
    observed = listing(config, key, budget, sha)
    if observed != version["objects"]:
        raise ValueError("object retention version changed")
    # Reserve a final list request. Leave time for both HEAD and DELETE at the
    # configured request pacing; network delays may still interrupt safely.
    available = min(
        (MAX_REQUESTS - budget.requests - 1) // 2,
        max(0, int((budget.deadline - time.monotonic() - 1) / 0.2)),
    )
    selected = dict(list(version.get("batch", observed).items())[: max(0, available)])
    if observed and not selected:
        return dict(status="budget", objects=0, bytes=0)
    for name, obj in selected.items():
        head = budget.call("head_object", Bucket=config["bucket"], Key=name)
        if (
            head["ContentLength"] != obj["size"]
            or head["ETag"] != obj["etag"]
            # HTTP Last-Modified has second precision; list XML may include ms.
            or int(head["LastModified"].timestamp()) != int(obj["modified"])
            or head.get("Metadata", {}).get("sha256")
            != version["files"][name]["sha256"]
        ):
            raise ValueError("object retention metadata mismatch")
    previous = receipt(config, key, sha, version["manifest_digest"])
    record = previous or dict(
        schema_version=1,
        key=key,
        sha=sha,
        status="deleting",
        manifest_digest=version["manifest_digest"],
        bucket_scope=bucket_scope(config),
        objects=observed,
    )
    path = receipt_path(config, key, sha)
    atomic(path, record)
    for name, obj in selected.items():
        # Current OtterIO ignores If-Match. This header is only extra protection
        # for compatible implementations, never the exclusive-writer safety gate.
        if progress is not None:
            progress["partial_possible"] = True
        response = budget.call(
            "delete_object", Bucket=config["bucket"], Key=name, IfMatch=obj["etag"]
        )
        if response.get("DeleteMarker") or response.get("VersionId") not in (
            None,
            "null",
        ):
            raise ValueError("unexpected versioned deletion")
        if progress is not None:
            progress["removed_objects"] += 1
            progress["removed_bytes"] += obj["size"]
    remaining = listing(config, key, budget, sha)
    expected = {name: obj for name, obj in observed.items() if name not in selected}
    if remaining != expected:
        raise ValueError("object retention version changed after deletion")
    if remaining:
        return dict(
            status="partial",
            objects=len(selected),
            bytes=sum(o["size"] for o in selected.values()),
        )
    # Completed receipts retain no object names and cannot authorize future deletion.
    atomic(
        path,
        dict(
            schema_version=1,
            key=key,
            sha=sha,
            status="complete",
            completed_at=time.time(),
        ),
    )
    if progress is not None:
        progress["removed_versions"] += 1
    return dict(
        status="complete",
        objects=len(observed),
        bytes=sum(o["size"] for o in observed.values()),
    )


def _run(config, key=None, apply=False, s3=None, now=None, progress=None):
    from .__main__ import atomic

    repositories = [r for r in config["repositories"] if key is None or r["key"] == key]
    if not repositories:
        raise ValueError("unknown object retention repository")
    for repository in repositories:
        settings = policy(config, repository)
        if apply and settings["enabled"] and not settings["exclusive_writer"]:
            raise ValueError(
                "object deletion requires explicit exclusive-writer ownership"
            )
    state = Path(config["state_root"])
    state.mkdir(parents=True, exist_ok=True)
    try:
        with contextlib.ExitStack() as stack:
            guard = stack.enter_context((state / "object-retention.lock").open("a"))
            fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
            stack.enter_context(local.cleanup_locks(config))
            budget = Budget(s3 or client(config))
            versioning = budget.call("get_bucket_versioning", Bucket=config["bucket"])
            if versioning.get("Status") not in (None, ""):
                raise ValueError("versioned or suspended buckets are unsupported")
            jobs = local.load_jobs(config, budget.deadline)
            plans = [
                plan(config, r, jobs, budget, now or time.time()) for r in repositories
            ]
            results = []
            for repository, value in zip(repositories, plans):
                row = {k: v for k, v in value.items() if k != "candidates"}
                row["candidates"] = [
                    dict(
                        sha=v["sha"],
                        objects=len(v["batch"]),
                        bytes=sum(o["size"] for o in v["batch"].values()),
                    )
                    for v in value["candidates"]
                ]
                row["removed_objects"] = row["removed_bytes"] = row[
                    "removed_versions"
                ] = 0
                if apply and value["enabled"]:
                    for version in value["candidates"]:
                        removed = remove(
                            config,
                            repository,
                            version,
                            budget,
                            progress.get(repository["key"]) if progress else None,
                        )
                        if removed["status"] in ("budget", "partial"):
                            row["skipped_versions"] += 1
                            reason = (
                                "batch-remaining"
                                if removed["status"] == "partial"
                                else "request-budget"
                            )
                            row["skip_counts"][reason] = (
                                row["skip_counts"].get(reason, 0) + 1
                            )
                        row["removed_objects"] += removed["objects"]
                        row["removed_bytes"] += removed["bytes"]
                        row["removed_versions"] += removed["status"] == "complete"
                results.append(row)
            report = dict(
                status="complete",
                apply=apply,
                repositories=results,
                completed_at=time.time(),
            )
            atomic(state / "object-retention-last.json", report)
            return report
    except BlockingIOError:
        return dict(status="busy", apply=apply, repositories=[])


def run(config, key=None, apply=False, s3=None, now=None):
    from .object_retention_info import observe, error_reason

    repositories = [r for r in config["repositories"] if key is None or r["key"] == key]
    if not repositories:
        raise ValueError("unknown object retention repository")
    started = time.time()
    mode = "apply" if apply else "preview"
    progress = {
        r["key"]: dict(
            removed_versions=0,
            removed_objects=0,
            removed_bytes=0,
            partial_possible=False,
        )
        for r in repositories
    }
    observe(config, repositories, started, mode, "running", "started")
    try:
        report = _run(config, key, apply, s3, now, progress)
    except Exception as error:
        observe(
            config,
            repositories,
            started,
            mode,
            "failed",
            error_reason(error),
            progress=progress,
        )
        raise
    observe(
        config,
        repositories,
        started,
        mode,
        report["status"],
        "busy" if report["status"] == "busy" else "preview-only",
        report=report,
    )
    return report


def main():
    from .__main__ import CONFIG, load

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--key")
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()
    try:
        config = load()
        scope(Path(CONFIG))
        report = run(config, args.key, args.apply)
    except Exception as error:
        print(
            json.dumps(dict(status="blocked", error=type(error).__name__)),
            file=sys.stderr,
        )
        raise SystemExit(1) from None
    print(json.dumps(report))


if __name__ == "__main__":
    main()
