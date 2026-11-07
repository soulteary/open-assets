"""Bounded, read-only closure observations with private audit evidence checks."""

import hashlib
import json
import os
import re
import stat
import time

import yaml

from .object_info import scope
from .object_retention import bucket_scope
from .restore_check import fingerprint, reference_fingerprint, source
from .restore_check_info import clean as clean_check
from .restore_close import REASONS
from .storage_info import finite_time, read_bytes, read_json

MAX_ATTEMPTS = 512
SCAN_SECONDS = 2


def header(value, key, audit_id):
    if (
        not isinstance(value, dict)
        or value.get("schema_version") != 1
        or value.get("key") != key
        or value.get("audit_id") != audit_id
        or not re.fullmatch(r"[0-9a-f]{40}", str(value.get("sha")))
        or not finite_time(value.get("started_at"))
    ):
        raise ValueError("invalid closure header")


def clean(intent, result, key, audit_id):
    header(intent, key, audit_id)
    if intent.get("status") != "running":
        raise ValueError("invalid closure intent")
    if result is None:
        return dict(status="running", started_at=intent["started_at"])
    header(result, key, audit_id)
    if (
        any(
            result.get(k) != intent.get(k)
            for k in ("sha", "started_at", "scope_digest")
        )
        or result.get("status") not in ("closed", "refused", "failed")
        or result.get("reason") not in REASONS
        or not finite_time(result.get("finished_at"))
        or result["finished_at"] < intent["started_at"]
    ):
        raise ValueError("invalid closure outcome")
    value = {k: result[k] for k in ("status", "reason", "started_at", "finished_at")}
    if result.get("source_status") in ("running", "failed", "complete"):
        value["source_status"] = result["source_status"]
    for k in ("source_uncertain", "current_is_target", "published_is_target"):
        if k in result:
            if type(result[k]) is not bool:
                raise ValueError("invalid closure flag")
            value[k] = result[k]
    if result["status"] == "closed" and (
        result.get("reason") != "verified"
        or result.get("source_uncertain") is not True
        or type(result.get("current_is_target")) is not bool
        or result.get("current_is_target") != result.get("published_is_target")
    ):
        raise ValueError("inconsistent closure")
    return value


def verify_evidence(directory, intent, result):
    verify_snapshots(
        read_bytes(directory / "source.json", 16384),
        read_bytes(directory / "check.json", 16384),
        intent,
        result,
    )


def verify_snapshots(source_raw, check_raw, intent, result):
    from .restore_info import clean as clean_restore

    if hashlib.sha256(source_raw).hexdigest() != result.get("source_report_digest"):
        raise ValueError("changed closure source snapshot")
    original = json.loads(source_raw)
    evidence = clean_restore(
        original, intent["key"], intent["sha"], intent["scope_digest"]
    )
    if (
        not evidence["uncertain"]
        or original.get("scope_digest") != intent["scope_digest"]
    ):
        raise ValueError("ineligible closure snapshot")
    if hashlib.sha256(check_raw).hexdigest() != result.get("check_digest"):
        raise ValueError("changed closure check snapshot")
    checked = json.loads(check_raw)
    sanitized = clean_check(checked, intent["key"], intent["sha"])
    if (
        sanitized["diagnosis"] != "verified"
        or checked.get("scope_digest") != intent["scope_digest"]
        or checked.get("source_uncertain") is not True
        or checked.get("source_report_digest") != result.get("source_report_digest")
        or any(
            checked.get(k) != result.get(k)
            for k in (
                "manifest_digest",
                "reference_digest",
                "current_is_target",
                "published_is_target",
            )
        )
        or checked["checks"]["references"]["reason"]
        not in ("target-current", "other-current")
        or checked["started_at"] < intent["started_at"]
        or checked["finished_at"] > result["finished_at"]
    ):
        raise ValueError("invalid closure check evidence")


def summary(root, data_root):
    try:
        config, keys, _ = scope(root / "private/repositories.yaml")
        if (
            config.get("state_root") != "/state"
            or config.get("images_root") != "/images"
        ):
            raise ValueError("unsupported closure mapping")
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
        parent = data_root / "publisher/restore-closures" / key
        count = 0
        try:
            for path in (parent.parent, parent):
                if path.exists() or path.is_symlink():
                    if not stat.S_ISDIR(path.lstat().st_mode):
                        raise ValueError("invalid closure directory")
            latest = None
            if parent.exists():
                with os.scandir(parent) as entries:
                    for index, entry in enumerate(entries):
                        count = index + 1
                        if index >= MAX_ATTEMPTS or time.monotonic() > deadline:
                            raise ValueError("closure scan limit")
                        if not re.fullmatch(
                            r"[0-9a-f]{32}", entry.name
                        ) or not entry.is_dir(follow_symlinks=False):
                            raise ValueError("invalid audit entry")
                        directory = parent / entry.name
                        intent = read_json(directory / "intent.json", 8192)
                        path = directory / "result.json"
                        result = (
                            read_json(path, 8192)
                            if path.exists() or path.is_symlink()
                            else None
                        )
                        record = clean(intent, result, key, entry.name)
                        if (
                            latest is None
                            or record["started_at"] > latest[0]["started_at"]
                        ):
                            latest = (record, intent, result, directory)
            row["online_attempts"] = count
            row["archive_hint"] = "near-limit" if count >= 448 else "none"
            if latest:
                record, intent, result, directory = latest
                if record["status"] == "closed":
                    verify_evidence(directory, intent, result)
                record["scope_changed"] = intent.get("scope_digest") != digest
                if result and result.get("source_report_digest"):
                    try:
                        current_source, _, _ = source(host, key, intent["sha"])
                        record["source_changed"] = (
                            current_source != result["source_report_digest"]
                        )
                        if result.get("manifest_digest"):
                            manifest = read_json(
                                data_root
                                / "publisher/manifests"
                                / key
                                / (intent["sha"] + ".json")
                            )
                            record["manifest_changed"] = (
                                fingerprint(manifest) != result["manifest_digest"]
                            )
                        if result.get("reference_digest"):
                            record["references_changed"] = (
                                reference_fingerprint(host, key)
                                != result["reference_digest"]
                            )
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
            row = dict(
                key=key,
                result_status="unavailable",
                online_attempts_at_least=count,
                archive_hint="scan-limit" if count > MAX_ATTEMPTS else "unavailable",
            )
        rows.append(row)
    return dict(status="ok", repositories=rows)
