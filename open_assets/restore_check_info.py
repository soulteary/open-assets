"""Filtered snapshots of explicit restoration checks; never rerun verification."""

import os
import re
import stat
import time

import yaml

from .object_info import scope
from .object_retention import bucket_scope
from .restore_check import CHECKS, fingerprint, reference_fingerprint
from .storage_info import finite_time, read_json

MAX_REPORTS = 512
SCAN_SECONDS = 2
STALE_SECONDS = 86400
REASONS = {
    "not-checked",
    "verified",
    "target-current",
    "other-current",
    "not-published",
    "none",
    "deleting",
    "complete",
    "restored",
    "missing",
    "permission",
    "timeout",
    "mismatch",
    "io",
    "storage",
    "interrupted",
    "unavailable",
}


def clean(value, key, sha):
    if (
        not isinstance(value, dict)
        or value.get("schema_version") != 1
        or value.get("key") != key
        or value.get("sha") != sha
    ):
        raise ValueError("invalid check identity")
    if (
        value.get("status") not in ("running", "complete", "failed")
        or value.get("diagnosis") not in ("verified", "issues", "unresolved")
        or value.get("phase") not in (*CHECKS, "validate", "complete")
        or not finite_time(value.get("started_at"))
    ):
        raise ValueError("invalid check status")
    result = {k: value[k] for k in ("status", "diagnosis", "phase", "started_at")}
    if value["status"] != "running":
        if (
            not finite_time(value.get("finished_at"))
            or value["finished_at"] < value["started_at"]
        ):
            raise ValueError("invalid check time")
        result["finished_at"] = value["finished_at"]
    checks = value.get("checks")
    if not isinstance(checks, dict) or set(checks) != set(CHECKS):
        raise ValueError("invalid check sections")
    result["checks"] = {}
    for name in CHECKS:
        item = checks[name]
        if (
            not isinstance(item, dict)
            or item.get("status") not in ("ok", "blocked", "failed", "unavailable")
            or item.get("reason") not in REASONS
        ):
            raise ValueError("invalid check result")
        result["checks"][name] = {k: item[k] for k in ("status", "reason")}
    for name in ("current_is_target", "published_is_target", "source_uncertain"):
        if name in value:
            if type(value[name]) is not bool:
                raise ValueError("invalid reference flag")
            result[name] = value[name]
    for name in ("files", "verified_objects"):
        if name in value:
            if type(value[name]) is not int or not 0 <= value[name] <= 10000:
                raise ValueError("invalid count")
            result[name] = value[name]
    if value.get("source_status") in (
        "missing",
        "unavailable",
        "running",
        "failed",
        "complete",
    ):
        result["source_status"] = value["source_status"]
    if value.get("failure_reason") in REASONS:
        result["failure_reason"] = value["failure_reason"]
    if value["diagnosis"] == "verified" and (
        value["status"] != "complete"
        or any(item["status"] != "ok" for item in result["checks"].values())
        or type(value.get("files")) is not int
        or value["files"] <= 0
        or value.get("verified_objects") != value["files"]
        or value.get("source_status") == "unavailable"
    ):
        raise ValueError("inconsistent verified result")
    return result


def summary(root, data_root, now=None):
    now = time.time() if now is None else now
    try:
        config, keys, _ = scope(root / "private/repositories.yaml")
        if (
            config.get("state_root") != "/state"
            or config.get("images_root") != "/images"
        ):
            raise ValueError("unsupported host mapping")
        host = dict(
            config,
            state_root=str(data_root / "publisher"),
            images_root=str(data_root / "images"),
        )
        digest = bucket_scope(config)
    except (OSError, ValueError, KeyError, TypeError, RecursionError, yaml.YAMLError):
        return dict(status="unavailable", repositories=[])
    rows = []
    deadline = time.monotonic() + SCAN_SECONDS
    for key in keys:
        row = dict(key=key, result_status="never")
        directory = data_root / "publisher/restore-checks" / key
        try:
            for path in (directory.parent, directory):
                if path.exists() or path.is_symlink():
                    if not stat.S_ISDIR(path.lstat().st_mode):
                        raise ValueError("invalid check directory")
            if directory.exists():
                latest = None
                with os.scandir(directory) as entries:
                    for index, entry in enumerate(entries):
                        if index >= MAX_REPORTS or time.monotonic() > deadline:
                            raise ValueError("check scan limit")
                        if re.fullmatch(r"tmp[a-z0-9_]{8}", entry.name):
                            continue
                        if not re.fullmatch(r"[0-9a-f]{40}\.json", entry.name):
                            raise ValueError("unexpected check entry")
                        value = read_json(directory / entry.name, 16384)
                        record = clean(value, key, entry.name[:-5])
                        if (
                            latest is None
                            or record["started_at"] > latest[0]["started_at"]
                        ):
                            latest = (record, value)
                if latest:
                    record, value = latest
                    sha = value["sha"]
                    record["scope_changed"] = value.get("scope_digest") != digest
                    record["age_seconds"] = max(
                        0, round(now - record.get("finished_at", record["started_at"]))
                    )
                    record["stale"] = record["age_seconds"] > STALE_SECONDS
                    try:
                        from .restore_check import source

                        current_source, _, _ = source(host, key, sha)
                        record["source_changed"] = current_source != value.get(
                            "source_report_digest"
                        )
                        manifest = read_json(
                            data_root / "publisher/manifests" / key / (sha + ".json")
                        )
                        record["manifest_changed"] = fingerprint(manifest) != value.get(
                            "manifest_digest"
                        )
                        record["references_changed"] = reference_fingerprint(
                            host, key
                        ) != value.get("reference_digest")
                    except (OSError, ValueError, TypeError, KeyError, RecursionError):
                        record["observation_unavailable"] = True
                    row.update(result_status="ok", attempt=record)
        except (
            OSError,
            ValueError,
            TypeError,
            KeyError,
            AttributeError,
            RecursionError,
        ):
            row = dict(key=key, result_status="unavailable")
        rows.append(row)
    return dict(status="ok", repositories=rows, stale_seconds=STALE_SECONDS)
