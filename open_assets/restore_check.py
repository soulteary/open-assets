"""Recheck restoration state without repairing objects, trees or activation records."""

import contextlib
import hashlib
import json
from pathlib import Path
import re
import time

from . import object_restore as restore, retention
from .object_retention import bucket_scope, receipt_path
from .storage_info import read_bytes

CHECKS = ("objects", "full", "images", "references", "barrier")


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def report_path(config, key, sha):
    return Path(config["state_root"]) / "restore-checks" / key / (sha + ".json")


def source(config, key, sha):
    path = restore.report_path(config, key, sha)
    for parent in (path.parent.parent, path.parent):
        if parent.exists() or parent.is_symlink():
            retention.real_directory(parent)
    if not path.exists() and not path.is_symlink():
        return None, "missing", {}
    raw = read_bytes(path, 16384)
    from .restore_info import clean

    value = json.loads(raw)
    cleaned = clean(value, key, sha, bucket_scope(config))
    if value.get("scope_digest") != bucket_scope(config):
        raise ValueError("restoration scope changed")
    return hashlib.sha256(raw).hexdigest(), value["status"], cleaned


def reference_fingerprint(config, key):
    import os

    base = Path(config["images_root"]) / "repos" / key
    current = base / "current"
    if current.is_symlink():
        target = os.readlink(current)
    elif current.exists():
        raise ValueError("current is not a link")
    else:
        target = None
    path = Path(config["state_root"]) / "published" / key
    published = (
        retention.read_json(path) if path.exists() or path.is_symlink() else None
    )
    return fingerprint([target, published])


def references(config, key, sha, report):
    state = Path(config["state_root"])
    base = Path(config["images_root"]) / "repos" / key
    for parent in (
        Path(config["images_root"]),
        base.parent,
        base,
        base / "releases",
        state / "published",
        state / "manifests",
        state / "manifests" / key,
    ):
        if parent.exists() or parent.is_symlink():
            retention.real_directory(parent)
    report["reference_digest"] = reference_fingerprint(config, key)
    current = retention.current_sha(base)
    published_path = state / "published" / key
    exists = published_path.exists() or published_path.is_symlink()
    published = retention.read_json(published_path) if exists else None
    report["current_is_target"] = current == sha
    report["published_is_target"] = (
        isinstance(published, dict) and published.get("sha") == sha
    )
    if current is None:
        if exists:
            raise ValueError("published without current")
        return "not-published"
    manifest = retention.read_json(state / "manifests" / key / (current + ".json"))
    restore.canonical_files(manifest, key, current)
    if published != manifest:
        raise ValueError("current and published manifest disagree")
    return "target-current" if current == sha else "other-current"


def barrier(config, key, sha):
    path = receipt_path(config, key, sha)
    for parent in (path.parent.parent, path.parent):
        if parent.exists() or parent.is_symlink():
            retention.real_directory(parent)
    if not path.exists() and not path.is_symlink():
        return "none"
    value = retention.read_json(path)
    if (
        value.get("schema_version") != 1
        or value.get("key") != key
        or value.get("sha") != sha
        or value.get("status") not in ("deleting", "complete", "restored")
    ):
        raise ValueError("invalid restoration barrier")
    return value["status"]


def reason(error):
    return {
        "FileNotFoundError": "missing",
        "PermissionError": "permission",
        "TimeoutError": "timeout",
        "ValueError": "mismatch",
        "OSError": "io",
        "ClientError": "storage",
        "KeyboardInterrupt": "interrupted",
    }.get(type(error).__name__, "unavailable")


def inspect(config, key, sha, report, s3, budget):
    from . import __main__ as core

    state = Path(config["state_root"])
    for parent in (state, state / "manifests", state / "manifests" / key):
        retention.real_directory(parent)
    original = retention.read_json(state / "manifests" / key / (sha + ".json"))
    files = restore.canonical_files(original, key, sha)
    report.update(manifest_digest=fingerprint(original), files=len(files))
    try:
        report["source_report_digest"], report["source_status"], evidence = source(
            config, key, sha
        )
        report["source_uncertain"] = evidence.get("uncertain", False)
    except (Exception, KeyboardInterrupt) as error:
        report.update(source_status="unavailable", source_reason=reason(error))

    def objects():
        expected = restore.expected_objects(original, key, sha)
        found = restore.listing_manifest(config, original, expected, s3, budget)
        if set(found) != set(expected):
            raise FileNotFoundError("incomplete object set")
        for name, item in expected.items():
            if not restore.metadata_matches(
                restore.get_verified(config, name, item, s3, budget), item
            ):
                raise ValueError("object metadata mismatch")
            report["verified_objects"] += 1
        return "verified"

    def tree(image=False):
        base = Path(config["images_root"]) / "repos" / key
        for path in (Path(config["images_root"]), base.parent, base, base / "releases"):
            retention.real_directory(path)
        core.verify_tree(
            base / "releases" / (("images-" if image else "") + sha),
            core.image_files(list(files.values())) if image else list(files.values()),
            check=budget.check,
        )
        return "verified"

    for name, operation in (
        ("objects", objects),
        ("full", tree),
        ("images", lambda: tree(True)),
        ("references", lambda: references(config, key, sha, report)),
        ("barrier", lambda: barrier(config, key, sha)),
    ):
        report["phase"] = name
        core.atomic(report_path(config, key, sha), report)
        try:
            budget.check()
            detail = operation()
            report["checks"][name] = dict(
                status="blocked"
                if name == "barrier" and detail in ("deleting", "complete")
                else "ok",
                reason=detail,
            )
        except (Exception, KeyboardInterrupt) as error:
            report["checks"][name] = dict(status="failed", reason=reason(error))
            if isinstance(error, KeyboardInterrupt):
                raise
    if retention.read_json(state / "manifests" / key / (sha + ".json")) != original:
        raise ValueError("manifest changed during check")
    report["diagnosis"] = (
        "verified"
        if all(v["status"] == "ok" for v in report["checks"].values())
        and report["source_status"] != "unavailable"
        else "issues"
    )


def run(config, key, sha, s3=None):
    from . import __main__ as core

    if (
        not isinstance(key, str)
        or not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", key)
        or not retention.valid_sha(sha)
    ):
        raise ValueError("invalid check identity")
    core.repo(config, key)
    budget = restore.Budget()
    with contextlib.ExitStack() as stack:
        try:
            stack.enter_context(core.lock(config, blocking=False))
        except BlockingIOError:
            return dict(status="busy", key=key, sha=sha)
        return run_locked(config, key, sha, s3, budget)


def run_locked(config, key, sha, s3=None, budget=None):
    """Caller holds Worker lock; retain the normal diagnostic report."""
    from . import __main__ as core

    budget = budget or restore.Budget()
    path = report_path(config, key, sha)
    restore.directories(path.parent.parent, path.parent)
    report = dict(
        schema_version=1,
        key=key,
        sha=sha,
        status="running",
        phase="validate",
        started_at=time.time(),
        scope_digest=bucket_scope(config),
        diagnosis="unresolved",
        verified_objects=0,
        checks={
            name: dict(status="unavailable", reason="not-checked") for name in CHECKS
        },
    )
    core.atomic(path, report)
    try:
        inspect(
            config,
            key,
            sha,
            report,
            s3 or core.s3client(config, bounded=True),
            budget,
        )
        report.update(status="complete", phase="complete")
    except (Exception, KeyboardInterrupt) as error:
        report.update(
            status="failed",
            diagnosis="unresolved",
            failure_reason=reason(error),
        )
    report["finished_at"] = time.time()
    core.atomic(path, report)
    return report
