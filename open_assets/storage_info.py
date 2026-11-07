"""Bounded, read-only local repository measurements for the host collector."""

import hashlib
import json
import math
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import time

import yaml

from . import retention

MAX_REPOSITORIES = 100
MAX_ENTRIES = 50000
SCAN_SECONDS = 2
CACHE_SECONDS = 300
METRICS = (
    "versions",
    "full_copies",
    "image_copies",
    "partial_versions",
    "local_bytes",
    "logical_bytes",
    "version_bytes",
    "git_bytes",
    "manifest_bytes",
    "trash_bytes",
    "staging_bytes",
    "other_bytes",
    "unusual_entries",
)
POLICY_FIELDS = (
    "enabled",
    "keep_last",
    "keep_days",
    "min_age_hours",
    "rollback_versions",
    "archive_jobs_after_days",
    "max_delete_versions",
    "max_delete_bytes",
    "max_archive_jobs",
)


def finite_time(value):
    return (
        type(value) in (int, float)
        and 0 <= value <= time.time() + 300
        and math.isfinite(value)
    )


def read_bytes(path, limit):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("nonregular storage metadata")
        raw = stream.read(limit + 1)
    if len(raw) > limit:
        raise ValueError("storage metadata exceeds limit")
    return raw


def read_json(path, limit=4194304):
    return json.loads(read_bytes(path, limit))


def write_cache(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream)
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def cached_metrics(value):
    if not isinstance(value, dict) or not finite_time(value.get("measured_at")):
        return None
    if not all(type(value.get(k)) is int and value[k] >= 0 for k in METRICS):
        return None
    return {k: value[k] for k in (*METRICS, "measured_at")}


def measure(data_root, key, deadline, counter):
    metrics = dict.fromkeys(METRICS, 0)
    seen = set()
    copies = {}
    base = data_root / "images/repos" / key
    roots = (
        (base, None),
        (data_root / "publisher/git" / key, "git_bytes"),
        (data_root / "publisher/manifests" / key, "manifest_bytes"),
    )
    for root, fixed_category in roots:
        # These fixed mappings never use paths supplied by repository metadata.
        for parent in (root.parent.parent, root.parent):
            if parent.exists() or parent.is_symlink():
                retention.real_directory(parent)
        if not root.exists() and not root.is_symlink():
            continue
        stack = [(root, ())]
        while stack:
            if time.monotonic() > deadline or counter[0] >= MAX_ENTRIES:
                raise TimeoutError("storage measurement budget exhausted")
            path, parts = stack.pop()
            counter[0] += 1
            entry = path.lstat()
            if not parts and not stat.S_ISDIR(entry.st_mode):
                raise ValueError("linked storage root")
            category = fixed_category or (
                "version_bytes"
                if parts and parts[0] == "releases"
                else "trash_bytes"
                if parts and parts[0] == ".retention-trash"
                else "staging_bytes"
                if parts and parts[0].startswith((".staging-", ".images-"))
                else "other_bytes"
            )
            identity = (entry.st_dev, entry.st_ino)
            if identity not in seen:
                seen.add(identity)
                if entry.st_blocks < 0:
                    raise ValueError("allocated size unavailable")
                allocated = entry.st_blocks * 512
                metrics[category] += allocated
                metrics["local_bytes"] += allocated
                if stat.S_ISREG(entry.st_mode):
                    metrics["logical_bytes"] += entry.st_size
            if stat.S_ISDIR(entry.st_mode):
                if (
                    fixed_category is None
                    and len(parts) == 2
                    and parts[0] == "releases"
                ):
                    match = re.fullmatch(r"(images-)?([0-9a-f]{40})", parts[1])
                    if match:
                        copies.setdefault(match[2], set()).add(
                            "images" if match[1] else "full"
                        )
                with os.scandir(path) as entries:
                    for child in entries:
                        if (
                            time.monotonic() > deadline
                            or counter[0] + len(stack) >= MAX_ENTRIES
                        ):
                            raise TimeoutError("storage measurement budget exhausted")
                        stack.append((Path(child.path), parts + (child.name,)))
            elif not stat.S_ISREG(entry.st_mode):
                if not (
                    fixed_category is None
                    and parts == ("current",)
                    and stat.S_ISLNK(entry.st_mode)
                ):
                    metrics["unusual_entries"] += 1
                # Never follow symlinks, FIFOs or devices, including current.
    metrics.update(
        versions=len(copies),
        full_copies=sum("full" in value for value in copies.values()),
        image_copies=sum("images" in value for value in copies.values()),
        partial_versions=sum(len(value) != 2 for value in copies.values()),
    )
    return metrics


def legacy_cleanup_results(data_root, keys):
    path = data_root / "publisher/retention-last.json"
    if not path.exists() and not path.is_symlink():
        return {key: dict(status="never") for key in keys}
    try:
        source = read_json(path)
        if (
            not isinstance(source, dict)
            or source.get("status") != "complete"
            or source.get("apply") is not True
            or not finite_time(source.get("completed_at"))
            or not isinstance(source.get("repositories"), list)
        ):
            raise ValueError("invalid cleanup result")
        result = {key: dict(status="not-in-last-run") for key in keys}
        for row in source["repositories"]:
            key = row.get("key") if isinstance(row, dict) else None
            if key not in result:
                continue
            if (
                type(row.get("enabled")) is not bool
                or not isinstance(row.get("candidates"), list)
                or len(row["candidates"]) > 100
                or not isinstance(row.get("archive_jobs"), list)
                or len(row["archive_jobs"]) > 1000
                or (row["enabled"] and "trash_removed" not in row)
                or type(row.get("trash_removed", 0)) is not int
                or not 0 <= row.get("trash_removed", 0) <= 100
            ):
                raise ValueError("invalid cleanup counts")
            result[key] = dict(
                status="complete" if row["enabled"] else "disabled",
                completed_at=source["completed_at"],
                claimed_versions=len(row["candidates"]) if row["enabled"] else 0,
                removed_directories=row.get("trash_removed", 0)
                if row["enabled"]
                else 0,
                archived_jobs=len(row["archive_jobs"]) if row["enabled"] else 0,
            )
        return result
    except (OSError, ValueError, KeyError, TypeError, RecursionError):
        return {key: dict(status="unavailable") for key in keys}


def cleanup_results(data_root, keys):
    """Prefer durable per-repository results; legacy global output is a fallback."""
    result = legacy_cleanup_results(data_root, keys)
    for key in keys:
        path = data_root / "publisher/retention-results" / (key + ".json")
        if not path.exists() and not path.is_symlink():
            continue
        try:
            source = read_json(path, 16384)
            if (
                not isinstance(source, dict)
                or source.get("schema_version") != 1
                or source.get("key") != key
                or source.get("status") != "complete"
                or source.get("apply") is not True
                or not finite_time(source.get("completed_at"))
            ):
                raise ValueError("invalid repository cleanup summary")
            for name, limit in (
                ("claimed_versions", 100),
                ("removed_directories", 100),
                ("archived_jobs", 1000),
            ):
                if type(source.get(name)) is not int or not 0 <= source[name] <= limit:
                    raise ValueError("invalid repository cleanup count")
            result[key] = {
                name: source[name]
                for name in (
                    "status",
                    "completed_at",
                    "claimed_versions",
                    "removed_directories",
                    "archived_jobs",
                )
            }
        except (OSError, ValueError, KeyError, TypeError, RecursionError):
            result[key] = dict(status="unavailable")
    return result


def cleanup_service(service="open-assets-retention.service"):
    fields = (
        "LoadState",
        "ConditionResult",
        "ActiveState",
        "Result",
        "ExecMainStatus",
        "ExecMainExitTimestampMonotonic",
    )
    try:
        output = subprocess.check_output(
            [
                "systemctl",
                "show",
                service,
                *["--property=" + name for name in fields],
            ],
            stderr=subprocess.DEVNULL,
            timeout=2,
        ).decode()
        values = dict(line.split("=", 1) for line in output.splitlines() if "=" in line)
        if values.get("LoadState") == "not-found":
            return dict(status="not-installed")
        if values.get("ConditionResult") == "no":
            return dict(status="condition-skipped")
        if values.get("ActiveState") in (
            "active",
            "activating",
            "reloading",
            "deactivating",
        ):
            return dict(status="running")
        if values.get("Result") != "success" or values.get("ActiveState") == "failed":
            return dict(status="failed")
        return dict(
            status="success"
            if int(values.get("ExecMainExitTimestampMonotonic", "0")) > 0
            else "never"
        )
    except (OSError, ValueError, subprocess.SubprocessError):
        return dict(status="unavailable")


def source_marker(data_root, key):
    values = []
    for path in (
        data_root / "images/repos" / key / "releases",
        data_root / "publisher/published" / key,
        data_root / "publisher/git" / key,
        data_root / "publisher/retention-last.json",
    ):
        try:
            values.append(path.lstat().st_mtime_ns)
        except FileNotFoundError:
            values.append(0)
    return values


def collect_storage(root, data_root, force=False, now=None):
    now = time.time() if now is None else now
    cache_path = data_root / "info/repositories-cache.json"
    try:
        raw = read_bytes(root / "private/repositories.yaml", 1048576)
        config = yaml.safe_load(raw)
        if (
            not isinstance(config, dict)
            or config.get("schema_version") != 1
            or config.get("state_root") != "/state"
            or config.get("images_root") != "/images"
            or not isinstance(config.get("repositories"), list)
            or len(config["repositories"]) > MAX_REPOSITORIES
        ):
            raise ValueError("unsupported storage mapping")
        repositories = config["repositories"]
        keys = [row.get("key") for row in repositories]
        if any(
            not isinstance(key, str) or not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", key)
            for key in keys
        ) or len(set(keys)) != len(keys):
            raise ValueError("invalid repository keys")
    except (
        OSError,
        ValueError,
        TypeError,
        AttributeError,
        RecursionError,
        yaml.YAMLError,
    ):
        return dict(status="unavailable", repositories=[])
    try:
        cache = read_json(cache_path)
        if not isinstance(cache, dict) or cache.get("schema_version") != 1:
            cache = {}
    except (OSError, ValueError, TypeError, RecursionError):
        cache = {}
    cached = cache.get("repositories", {})
    if not isinstance(cached, dict):
        cached = {}
    digest = hashlib.sha256(raw).hexdigest()
    refresh = force or cache.get("config_digest") != digest
    deadline, counter = time.monotonic() + SCAN_SECONDS, [0]
    completed = cleanup_results(data_root, keys)
    rows, measurements, markers, failures = [], {}, {}, []
    old_markers = cache.get("markers", {})
    if not isinstance(old_markers, dict):
        old_markers = {}
    old_failures = cache.get("failures", [])
    if not isinstance(old_failures, list):
        old_failures = []
    host_config = dict(config, state_root=str(data_root / "publisher"))
    for repository in repositories:
        key = repository["key"]
        row = dict(key=key, cleanup=completed[key])
        try:
            settings = retention.policy(config, repository)
            row["policy"] = {name: settings[name] for name in POLICY_FIELDS}
            row["policy"]["pinned_count"] = len(set(settings["pinned_shas"]))
            row["rollback_records"] = len(set(retention.history(host_config, key)))
            quota = repository.get("max_bytes", 1073741824)
            if type(quota) is not int or not 0 < quota <= 10**15:
                raise ValueError("invalid publication reserve")
            row["publication_required_bytes"] = quota * 2
        except (OSError, ValueError, KeyError, TypeError, RecursionError):
            row["policy_status"] = "unavailable"
        old = cached_metrics(cached.get(key))
        try:
            marker = source_marker(data_root, key)
            if (
                refresh
                or old is None
                or key in old_failures
                or old_markers.get(key) != marker
                or now - old["measured_at"] >= CACHE_SECONDS
            ):
                old = dict(measure(data_root, key, deadline, counter), measured_at=now)
            markers[key] = marker
            row.update(old, measurement_status="ok")
        except (OSError, ValueError, TypeError):
            row["measurement_status"] = "unavailable"
            failures.append(key)
            if old is not None:
                row.update(old)
        if old is not None:
            measurements[key] = old
            row["measurement_age_seconds"] = max(0, round(now - old["measured_at"]))
            row["measurement_stale"] = (
                row["measurement_age_seconds"] > CACHE_SECONDS * 2
                or row["measurement_status"] != "ok"
            )
        rows.append(row)
    try:
        write_cache(
            cache_path,
            dict(
                schema_version=1,
                config_digest=digest,
                repositories=measurements,
                markers=markers,
                failures=failures,
            ),
        )
    except OSError:
        pass  # An unwritable cache must not invalidate a measured snapshot.
    return dict(
        status="ok"
        if all(
            row["measurement_status"] == "ok"
            and row.get("policy_status") != "unavailable"
            for row in rows
        )
        else "partial",
        repositories=rows,
    )


THRESHOLDS = dict(
    INFO_DISK_WARN_PERCENT=85,
    INFO_DISK_CRITICAL_PERCENT=95,
    INFO_DISK_WARN_FREE_BYTES=5368709120,
    INFO_DISK_CRITICAL_FREE_BYTES=2147483648,
    INFO_INODE_WARN_PERCENT=85,
    INFO_INODE_CRITICAL_PERCENT=95,
)


def capacity(disks, storage, env, data_disk_index=0):
    try:
        limits = {
            name: int(env.get(name, default)) for name, default in THRESHOLDS.items()
        }
        for prefix in ("INFO_DISK", "INFO_INODE"):
            if (
                not 0
                < limits[prefix + "_WARN_PERCENT"]
                < limits[prefix + "_CRITICAL_PERCENT"]
                <= 100
            ):
                raise ValueError("invalid capacity threshold")
        if (
            not 0
            <= limits["INFO_DISK_CRITICAL_FREE_BYTES"]
            < limits["INFO_DISK_WARN_FREE_BYTES"]
            <= 9007199254740991
        ):
            raise ValueError("invalid free-space threshold")
    except (ValueError, TypeError):
        return dict(
            level="unknown",
            alerts=[dict(code="invalid-thresholds", severity="warning")],
        )
    alerts = []
    for index, value in enumerate(disks):
        for resource, warn, critical in (
            (
                "space",
                limits["INFO_DISK_WARN_PERCENT"],
                limits["INFO_DISK_CRITICAL_PERCENT"],
            ),
            (
                "inode",
                limits["INFO_INODE_WARN_PERCENT"],
                limits["INFO_INODE_CRITICAL_PERCENT"],
            ),
        ):
            percent = value.get(
                "used_percent" if resource == "space" else "inode_used_percent"
            )
            if percent is None:
                alerts.append(
                    dict(code="inode-unavailable", severity="warning", disk=index)
                )
            elif percent >= critical:
                alerts.append(
                    dict(code=resource + "-critical", severity="critical", disk=index)
                )
            elif percent >= warn:
                alerts.append(
                    dict(code=resource + "-warning", severity="warning", disk=index)
                )
        free = value["available"]
        if free < limits["INFO_DISK_CRITICAL_FREE_BYTES"]:
            alerts.append(dict(code="free-critical", severity="critical", disk=index))
        elif free < limits["INFO_DISK_WARN_FREE_BYTES"]:
            alerts.append(dict(code="free-warning", severity="warning", disk=index))
    for row in storage.get("repositories", []):
        required = row.get("publication_required_bytes")
        if required and disks[data_disk_index]["available"] < required:
            alerts.append(
                dict(
                    code="publication-reserve",
                    severity="critical",
                    key=row["key"],
                    required_bytes=required,
                )
            )
    if storage.get("status") != "ok":
        alerts.append(dict(code="storage-unavailable", severity="warning"))
    elif any(row.get("measurement_stale") for row in storage.get("repositories", [])):
        alerts.append(dict(code="storage-stale", severity="warning"))
    level = (
        "critical"
        if any(a["severity"] == "critical" for a in alerts)
        else "warning"
        if alerts
        else "ok"
    )
    return dict(level=level, alerts=alerts, thresholds=limits)
