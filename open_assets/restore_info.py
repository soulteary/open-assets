"""Read bounded restoration reports for Info without Git, S3 or state writes."""

import os
import re
import stat
import time

import yaml

from .object_info import scope
from .object_retention import bucket_scope
from .storage_info import finite_time, read_json

MAX_REPORTS = 512
SCAN_SECONDS = 2
PHASES = {
    "validate",
    "git",
    "export",
    "local-preflight",
    "preflight",
    "upload",
    "verify",
    "local-install",
    "activate",
    "complete",
}
FLAGS = (
    "partial_possible",
    "with_local",
    "rollback_requested",
    "local_full_rebuilt",
    "local_images_rebuilt",
    "local_verified",
    "local_partial_possible",
    "activated",
    "activation_uncertain",
)


def clean(value, key, sha, digest):
    if (
        not isinstance(value, dict)
        or value.get("schema_version") != 1
        or value.get("key") != key
        or value.get("sha") != sha
    ):
        raise ValueError("invalid restoration identity")
    if (
        value.get("status") not in ("running", "complete", "failed")
        or value.get("phase") not in PHASES
        or not finite_time(value.get("started_at"))
    ):
        raise ValueError("invalid restoration status")
    result = {k: value[k] for k in ("status", "phase", "started_at")}
    if value["status"] != "running":
        if (
            not finite_time(value.get("finished_at"))
            or value["finished_at"] < value["started_at"]
        ):
            raise ValueError("invalid completion time")
        result["finished_at"] = value["finished_at"]
    for name in FLAGS:
        if name in value:
            if type(value[name]) is not bool:
                raise ValueError("invalid restoration flag")
            result[name] = value[name]
    for name in ("files", "verified_objects", "uploaded_objects", "repaired_objects"):
        if name in value:
            if type(value[name]) is not int or not 0 <= value[name] <= 10000:
                raise ValueError("invalid restoration count")
            result[name] = value[name]
    if value["status"] == "failed":
        result["failure_reason"] = {
            "ValueError": "validation",
            "OSError": "io",
            "PermissionError": "permission",
            "FileNotFoundError": "missing",
            "TimeoutError": "timeout",
            "TimeoutExpired": "timeout",
            "CalledProcessError": "git",
            "ClientError": "storage",
            "KeyboardInterrupt": "interrupted",
        }.get(
            value.get("error") if isinstance(value.get("error"), str) else None,
            "unknown",
        )
    result["scope_changed"] = value.get("scope_digest") != digest
    result["objects_verified"] = (
        value["phase"] in ("local-install", "activate", "complete")
        and value.get("files") == value.get("verified_objects")
        and type(value.get("files")) is int
    )
    result["uncertain"] = (
        value["status"] == "running"
        or bool(value.get("activation_uncertain"))
        or (
            value["status"] != "complete"
            and any(
                value.get(k) for k in ("partial_possible", "local_partial_possible")
            )
        )
    )
    if value["status"] == "complete" and (
        value["phase"] != "complete" or not result["objects_verified"]
    ):
        raise ValueError("inconsistent completion")
    if value.get("activated") and not all(
        value.get(k) is True
        for k in ("rollback_requested", "with_local", "local_verified")
    ):
        raise ValueError("inconsistent activation")
    return result


def summary(root, data_root):
    try:
        config, keys, _ = scope(root / "private/repositories.yaml")
        if config.get("state_root") != "/state":
            raise ValueError("unsupported state mapping")
        digest = bucket_scope(config)
    except (OSError, ValueError, KeyError, TypeError, RecursionError, yaml.YAMLError):
        return dict(status="unavailable", repositories=[])
    rows = []
    deadline = time.monotonic() + SCAN_SECONDS
    for key in keys:
        row = dict(key=key, result_status="never")
        directory = data_root / "publisher/object-restores" / key
        try:
            parent = directory.parent
            if parent.is_symlink() or (
                parent.exists() and not stat.S_ISDIR(parent.lstat().st_mode)
            ):
                raise ValueError("invalid report root")
            if directory.exists() or directory.is_symlink():
                if not stat.S_ISDIR(directory.lstat().st_mode):
                    raise ValueError("invalid report directory")
                latest = None
                with os.scandir(directory) as entries:
                    for index, entry in enumerate(entries):
                        if index >= MAX_REPORTS or time.monotonic() > deadline:
                            raise ValueError("report scan limit")
                        if not re.fullmatch(r"[0-9a-f]{40}\.json", entry.name):
                            # Atomic writer temporary files are not completed reports.
                            if re.fullmatch(r"tmp[a-z0-9_]{8}", entry.name):
                                continue
                            raise ValueError("unexpected report entry")
                        record = clean(
                            read_json(directory / entry.name, 16384),
                            key,
                            entry.name[:-5],
                            digest,
                        )
                        if (
                            latest is None
                            or record["started_at"] > latest["started_at"]
                        ):
                            latest = record
                if latest is not None:
                    row.update(result_status="ok", attempt=latest)
        except (
            OSError,
            ValueError,
            KeyError,
            TypeError,
            AttributeError,
            RecursionError,
        ):
            # Never fall back to an older green result when the latest is unreadable.
            row = dict(key=key, result_status="unavailable")
        rows.append(row)
    return dict(status="ok", repositories=rows)
