"""Conservative local retention. Never delete S3 objects or repository history."""

import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import stat
import sys
import time
import uuid

DEFAULTS = dict(
    enabled=False,
    keep_last=5,
    keep_days=30,
    min_age_hours=24,
    rollback_versions=2,
    pinned_shas=[],
    archive_jobs_after_days=30,
    max_delete_versions=10,
    max_delete_bytes=268435456,
    max_archive_jobs=100,
)
SHA = re.compile(r"[0-9a-f]{40}")
MAX_ACTIVE_JOBS = 10000


def policy(config, repository):
    result = dict(DEFAULTS)
    for source in (config.get("retention", {}), repository.get("retention", {})):
        if not isinstance(source, dict) or set(source) - set(DEFAULTS):
            raise ValueError("invalid retention options")
        result.update(source)
    if type(result["enabled"]) is not bool:
        raise ValueError("retention enabled must be boolean")
    limits = dict(
        keep_last=(1, 10000),
        keep_days=(0, 36500),
        min_age_hours=(1, 876000),
        rollback_versions=(1, 100),
        archive_jobs_after_days=(1, 36500),
        max_delete_versions=(1, 100),
        max_delete_bytes=(1, 10737418240),
        max_archive_jobs=(1, 1000),
    )
    for name, (low, high) in limits.items():
        if type(result[name]) is not int or not low <= result[name] <= high:
            raise ValueError("invalid retention limit: " + name)
    pins = result["pinned_shas"]
    if (
        not isinstance(pins, list)
        or len(pins) > 1000
        or any(not valid_sha(s) for s in pins)
    ):
        raise ValueError("invalid retention pins")
    return result


def valid_sha(value):
    return (
        isinstance(value, str)
        and SHA.fullmatch(value) is not None
        and value != "0" * 40
    )


def read_json(path, limit=4194304):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("retention metadata is not regular")
        raw = stream.read(limit + 1)
    if len(raw) > limit:
        raise ValueError("retention metadata too large")
    return json.loads(raw)


def real_directory(path):
    if not stat.S_ISDIR(path.lstat().st_mode):
        raise ValueError("retention directory is linked or invalid")


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def history_path(config, key):
    return Path(config["state_root"]) / "rollback-protection" / (key + ".json")


def history(config, key):
    path = history_path(config, key)
    if not path.exists() and not path.is_symlink():
        return []
    value = read_json(path, 16384)
    if (
        not isinstance(value, dict)
        or value.get("key") != key
        or not isinstance(value.get("shas"), list)
        or len(value["shas"]) > 100
        or any(not valid_sha(s) for s in value["shas"])
    ):
        raise ValueError("invalid rollback protection")
    return value["shas"]


def current_sha(base, check_target=True):
    path = base / "current"
    if not path.is_symlink():
        if path.exists():
            raise ValueError("current must be a symlink")
        return None
    target = os.readlink(path)
    sha = activation_sha(target)
    if check_target:
        if target.endswith("/images"):
            real_directory(base / "releases" / sha)
        real_directory(base / target)
    return sha


def activation_sha(target):
    match = re.fullmatch(
        r"releases/(?:images-([0-9a-f]{40})|([0-9a-f]{40})/images)", target
    )
    sha = (match[1] or match[2]) if match else None
    if not valid_sha(sha):
        raise ValueError("invalid activation target")
    return sha


def remember_previous(config, key, next_sha):
    """Persist protection before activation; failed activation may over-retain."""
    from . import __main__ as core

    repository = next(r for r in config["repositories"] if r["key"] == key)
    count = policy(config, repository)["rollback_versions"]
    base = Path(config["images_root"]) / "repos" / key
    previous = current_sha(base, check_target=False)
    # Directory timestamps record recent use without changing immutable bytes.
    # Touch both sides before switching; an unsuccessful attempt only over-retains.
    for sha in {previous, next_sha} - {None}:
        for prefix in ("", "images-"):
            path = base / "releases" / (prefix + sha)
            if path.exists() or path.is_symlink():
                real_directory(path)
                os.utime(path, None)
    values = ([previous] if previous else []) + history(config, key)
    values = list(dict.fromkeys(s for s in values if s != next_sha))[:count]
    core.atomic(history_path(config, key), dict(key=key, shas=values))


def archive_path(config, job_id):
    return Path(config["state_root"]) / "job-archive" / job_id[:2] / (job_id + ".json")


def check_deadline(deadline):
    if deadline is not None and time.monotonic() > deadline:
        raise TimeoutError("retention planning exceeded its time budget")


def tree_stats(path, deadline=None):
    real_directory(path)
    size, newest = 0, path.stat().st_mtime
    for directory, directories, names in os.walk(
        path, followlinks=False, onerror=raise_walk_error
    ):
        check_deadline(deadline)
        for name in directories:
            check_deadline(deadline)
            child = Path(directory) / name
            real_directory(child)
            newest = max(newest, child.stat().st_mtime)
        for name in names:
            check_deadline(deadline)
            entry = (Path(directory) / name).lstat()
            if not stat.S_ISREG(entry.st_mode):
                raise ValueError("nonregular version entry")
            size += entry.st_size
            newest = max(newest, entry.st_mtime)
    return size, newest


def raise_walk_error(error):
    raise error


def load_jobs(config, deadline=None):
    from . import __main__ as core

    jobs = Path(config["state_root"]) / "jobs"
    if not jobs.exists():
        return []
    real_directory(jobs)
    # Invalid live records stop cleanup. The Worker can quarantine them separately.
    result = []
    for path in jobs.glob("*.json"):
        check_deadline(deadline)
        if len(result) >= MAX_ACTIVE_JOBS:
            raise ValueError("active task scan limit exceeded")
        job = core.read_job(path)
        previous = job.get("previous")
        if previous is not None and (
            not isinstance(previous, str) or len(previous) > 64
        ):
            raise ValueError("invalid previous version reference")
        finished = job.get("finished", 0)
        if type(finished) not in (int, float) or not 0 <= finished <= 1e12:
            raise ValueError("invalid task completion time")
        fields = ("id", "key", "sha", "status", "created", "finished", "previous")
        result.append((path, {name: job[name] for name in fields if name in job}))
    return result


def protected_references(config, key, jobs, pins, deadline=None):
    """Shared safety references for local and object retention; invalid state blocks both."""
    base = Path(config["images_root"]) / "repos" / key
    for parent in (Path(config["images_root"]), base.parent, base):
        if parent.exists() or parent.is_symlink():
            real_directory(parent)
    current = current_sha(base)
    published_path = Path(config["state_root"]) / "published" / key
    if published_path.exists() or published_path.is_symlink():
        published = read_json(published_path)
        if (
            not isinstance(published, dict)
            or published.get("key") != key
            or not valid_sha(published.get("sha"))
            or published.get("sha") != current
        ):
            raise ValueError("current and published record disagree")
    elif current is not None:
        raise ValueError("current has no published record")
    protected = {}

    def protect(sha, reason):
        protected.setdefault(sha, []).append(reason)

    if current:
        protect(current, "current")
    for sha in history(config, key):
        protect(sha, "rollback")
    for sha in pins:
        protect(sha, "pinned")
    for _, job in jobs:
        check_deadline(deadline)
        if job["key"] != key:
            continue
        if job["status"] in ("pending", "running", "images_prepared", "failed"):
            protect(job["sha"], "job-" + job["status"])
        # Bootstrap protection for deployments predating activation history.
        if (
            job["sha"] == current
            or job["status"] in ("pending", "running", "images_prepared", "failed")
        ) and job.get("previous"):
            protect(activation_sha(job["previous"]), "previous-job")
    return current, protected


def plan_repository(config, repository, jobs, now, deadline=None):
    check_deadline(deadline)
    key = repository["key"]
    settings = policy(config, repository)
    result = dict(
        key=key,
        enabled=settings["enabled"],
        protected={},
        candidates=[],
        archive_jobs=[],
        local_bytes=0,
        scope="local-only",
    )
    base = Path(config["images_root"]) / "repos" / key
    for parent in (Path(config["images_root"]), base.parent):
        if parent.exists() or parent.is_symlink():
            real_directory(parent)
    if not base.exists() and not base.is_symlink():
        return result
    real_directory(base)
    trash = base / ".retention-trash"
    if trash.exists() or trash.is_symlink():
        real_directory(trash)
        for folder in trash.iterdir():
            check_deadline(deadline)
            inspect_claim(folder, key, deadline)
    current, protected = protected_references(
        config, key, jobs, settings["pinned_shas"], deadline
    )

    def protect(sha, reason):
        protected.setdefault(sha, []).append(reason)

    releases = base / "releases"
    if not releases.exists():
        result["protected"] = protected
        return result
    real_directory(releases)
    versions = {}
    for path in releases.iterdir():
        check_deadline(deadline)
        match = re.fullmatch(r"(?:images-)?([0-9a-f]{40})", path.name)
        if not match:
            continue
        sha = match[1]
        if not valid_sha(sha):
            raise ValueError("invalid version directory")
        size, newest = tree_stats(path, deadline)
        entry = versions.setdefault(sha, dict(sha=sha, bytes=0, newest=0))
        entry["bytes"] += size
        entry["newest"] = max(entry["newest"], newest)
    if current is None:
        for sha in versions:
            protect(sha, "no-current-reference")
    for sha, entry in versions.items():
        check_deadline(deadline)
        manifest_path = Path(config["state_root"]) / "manifests" / key / (sha + ".json")
        if not manifest_path.exists() and not manifest_path.is_symlink():
            protect(sha, "missing-manifest")
            continue
        manifest = read_json(manifest_path)
        if (
            not isinstance(manifest, dict)
            or manifest.get("key") != key
            or manifest.get("sha") != sha
        ):
            raise ValueError("invalid version manifest identity")
        entry["newest"] = max(entry["newest"], manifest_path.stat().st_mtime)
    for _, job in jobs:
        check_deadline(deadline)
        if job["key"] == key and job["sha"] in versions:
            # Never let a recently queued or retried version look artificially old.
            timestamp = max(job["created"], job.get("finished", 0))
            if type(timestamp) not in (int, float) or not 0 <= timestamp <= now + 86400:
                raise ValueError("invalid version job time")
            versions[job["sha"]]["newest"] = max(
                versions[job["sha"]]["newest"], timestamp
            )
    ordered = sorted(
        versions.values(),
        key=lambda entry: (entry["newest"], entry["sha"]),
        reverse=True,
    )
    for entry in ordered[: settings["keep_last"]]:
        protect(entry["sha"], "keep-last")
    threshold = max(settings["keep_days"] * 86400, settings["min_age_hours"] * 3600)
    for entry in ordered:
        if now - entry["newest"] < threshold:
            protect(entry["sha"], "recent")
    eligible = [e for e in reversed(ordered) if e["sha"] not in protected]
    total = 0
    for entry in eligible:
        if len(result["candidates"]) >= settings["max_delete_versions"]:
            break
        if total + entry["bytes"] > settings["max_delete_bytes"]:
            continue
        result["candidates"].append(entry)
        total += entry["bytes"]
    for path, job in jobs:
        check_deadline(deadline)
        if (
            job["key"] == key
            and job["status"] in ("published", "superseded")
            and job["sha"] not in protected
            and now - max(job["created"], job.get("finished", 0), path.stat().st_mtime)
            >= settings["archive_jobs_after_days"] * 86400
        ):
            result["archive_jobs"].append(job["id"])
    result["archive_jobs"] = sorted(result["archive_jobs"])[
        : settings["max_archive_jobs"]
    ]
    for job_id in result["archive_jobs"]:
        check_deadline(deadline)
        target = archive_path(config, job_id)
        for parent in (target.parent.parent, target.parent):
            if parent.exists() or parent.is_symlink():
                real_directory(parent)
        if target.exists() or target.is_symlink():
            raise ValueError("duplicate archived task")
    result.update(protected=protected, local_bytes=sum(e["bytes"] for e in ordered))
    return result


@contextlib.contextmanager
def cleanup_locks(config):
    """Do not wait behind publication. Enqueue is blocked only during claims."""
    state = Path(config["state_root"])
    state.mkdir(parents=True, exist_ok=True)
    with (
        (state / "worker.lock").open("a") as worker,
        (state / "queue.lock").open("a") as queue,
    ):
        fcntl.flock(worker, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(queue, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield


def claim_versions(config, repository, plan):
    from . import __main__ as core

    key = repository["key"]
    base = Path(config["images_root"]) / "repos" / key
    trash = base / ".retention-trash"
    if plan["candidates"]:
        trash.mkdir(exist_ok=True, mode=0o700)
        real_directory(trash)
    for entry in plan["candidates"]:
        folder = trash / uuid.uuid4().hex
        folder.mkdir(mode=0o700)
        core.atomic(
            folder / "receipt.json", dict(schema_version=1, key=key, sha=entry["sha"])
        )
        sync_directory(trash)
        for prefix, target in (("", "full"), ("images-", "images")):
            source = base / "releases" / (prefix + entry["sha"])
            if source.exists() or source.is_symlink():
                real_directory(source)
                os.rename(source, folder / target)
        sync_directory(folder)
        sync_directory(base / "releases")
    for job_id in plan["archive_jobs"]:
        source = Path(config["state_root"]) / "jobs" / (job_id + ".json")
        target = archive_path(config, job_id)
        archive_root = target.parent.parent
        archive_root.mkdir(exist_ok=True)
        real_directory(archive_root)
        target.parent.mkdir(exist_ok=True)
        real_directory(target.parent)
        if target.exists() or target.is_symlink():
            if core.read_job(source) != core.read_job(target):
                raise ValueError("conflicting archived task")
            # Keep the duplicate record for inspection; never silently overwrite it.
            raise ValueError("duplicate archived task")
        os.rename(source, target)
        sync_directory(target.parent)
        sync_directory(source.parent)


def inspect_claim(folder, key, deadline=None):
    if not re.fullmatch(r"[0-9a-f]{32}", folder.name):
        raise ValueError("unrecognized trash entry")
    real_directory(folder)
    if not list(folder.iterdir()):
        return None
    receipt = read_json(folder / "receipt.json", 16384)
    if (
        not isinstance(receipt, dict)
        or receipt.get("schema_version") != 1
        or receipt.get("key") != key
        or not valid_sha(receipt.get("sha"))
    ):
        raise ValueError("invalid cleanup receipt")
    if set(p.name for p in folder.iterdir()) - {"receipt.json", "full", "images"}:
        raise ValueError("unexpected trash content")
    return sum(
        tree_stats(folder / name, deadline)[0]
        for name in ("full", "images")
        if (folder / name).exists() or (folder / name).is_symlink()
    )


def drain_trash(config, repository):
    """Delete claimed trees only. Never complete old claims against live paths."""
    key = repository["key"]
    trash = Path(config["images_root"]) / "repos" / key / ".retention-trash"
    if not trash.exists() and not trash.is_symlink():
        return 0
    real_directory(trash)
    deleted, total = 0, 0
    settings = policy(config, repository)
    for folder in sorted(trash.iterdir()):
        if deleted >= settings["max_delete_versions"]:
            break
        size = inspect_claim(folder, key)
        if size is None:
            folder.rmdir()
            sync_directory(trash)
            continue
        if total + size > settings["max_delete_bytes"]:
            continue
        # rmtree unlinks child symlinks rather than following them. Reject a linked root.
        for name in ("full", "images"):
            path = folder / name
            if path.exists() or path.is_symlink():
                real_directory(path)
                shutil.rmtree(path)
        (folder / "receipt.json").unlink()
        folder.rmdir()
        sync_directory(trash)
        deleted += 1
        total += size
    return deleted


def _run_locked(config, key=None, apply=False, now=None):
    now = time.time() if now is None else now
    repositories = [r for r in config["repositories"] if key is None or r["key"] == key]
    if not repositories:
        raise ValueError("unknown repository")
    plans = []
    try:
        with cleanup_locks(config):
            deadline = time.monotonic() + 30
            jobs = load_jobs(config, deadline)
            for repository in repositories:
                plan = plan_repository(config, repository, jobs, now, deadline)
                plans.append(plan)
            if apply:
                for repository, plan in zip(repositories, plans):
                    if plan["enabled"]:
                        claim_versions(config, repository, plan)
        # Once claimed, paths are outside releases. A new publication can safely
        # rebuild the same SHA while old trash is being removed.
        if apply:
            for repository, plan in zip(repositories, plans):
                if plan["enabled"]:
                    plan["trash_removed"] = drain_trash(config, repository)
    except BlockingIOError:
        return dict(status="busy", apply=apply, repositories=[])
    report = dict(status="complete", apply=apply, repositories=plans)
    if apply:
        from . import __main__ as core

        completed_at = time.time()
        for row in plans:
            if row["enabled"]:
                core.atomic(
                    Path(config["state_root"])
                    / "retention-results"
                    / (row["key"] + ".json"),
                    dict(
                        schema_version=1,
                        key=row["key"],
                        status="complete",
                        apply=True,
                        completed_at=completed_at,
                        claimed_versions=len(row["candidates"]),
                        removed_directories=row.get("trash_removed", 0),
                        archived_jobs=len(row["archive_jobs"]),
                    ),
                )
        core.atomic(
            Path(config["state_root"]) / "retention-last.json",
            dict(report, completed_at=completed_at),
        )
    return report


def run(config, key=None, apply=False, now=None):
    state = Path(config["state_root"])
    state.mkdir(parents=True, exist_ok=True)
    try:
        with (state / "retention.lock").open("a") as guard:
            fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return _run_locked(config, key, apply, now)
    except BlockingIOError:
        return dict(status="busy", apply=apply, repositories=[])


def main():
    from . import __main__ as core

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--key")
    parser.add_argument(
        "--apply",
        action="store_true",
        help="apply only repositories with retention.enabled=true",
    )
    args = parser.parse_args()
    try:
        report = run(core.load(), args.key, args.apply)
    except (OSError, ValueError, KeyError, TypeError, RecursionError) as error:
        print(
            json.dumps(dict(status="blocked", error=type(error).__name__)),
            file=sys.stderr,
        )
        raise SystemExit(1) from None
    print(json.dumps(report, ensure_ascii=False))


if __name__ == "__main__":
    main()
