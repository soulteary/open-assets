"""Durable, filtered per-repository retention observations; no S3 requests."""

import fcntl
import hashlib
import json
from pathlib import Path
import re
import time

from .storage_info import finite_time, read_json

import yaml

REASONS = {
    "started",
    "preview-only",
    "disabled",
    "no-candidates",
    "applied",
    "budget-skipped",
    "busy",
    "exclusive-writer-required",
    "versioned-bucket",
    "time-budget",
    "scan-limit",
    "changed-objects",
    "permission-denied",
    "storage-error",
    "unsafe-state",
    "io-error",
    "unknown-error",
}
PROTECTIONS = {
    "current",
    "rollback",
    "pinned",
    "previous-job",
    "keep-last",
    "recent",
    "no-current-reference",
    "missing-manifest",
    "incomplete-or-extra-objects",
    "manifest-size-mismatch",
    "job-pending",
    "job-running",
    "job-images_prepared",
    "job-failed",
}
COUNTS = (
    "candidate_versions",
    "candidate_objects",
    "candidate_bytes",
    "eligible_versions",
    "eligible_objects",
    "eligible_bytes",
    "unknown_objects",
    "skipped_versions",
    "removed_versions",
    "removed_objects",
    "removed_bytes",
)
STALE_SECONDS = 12 * 3600


def fingerprint(config, repository):
    from .object_retention import policy
    from .retention import policy as local_policy

    return hashlib.sha256(
        json.dumps(
            [
                config.get("endpoint"),
                config.get("bucket"),
                repository["key"],
                policy(config, repository),
                local_policy(config, repository),
            ],
            sort_keys=True,
        ).encode()
    ).hexdigest()


def clean_record(value, complete=False):
    if not isinstance(value, dict) or value.get("mode") not in ("preview", "apply"):
        return None
    if not finite_time(value.get("started_at")) or not finite_time(
        value.get("finished_at")
    ):
        return None
    if (
        value["finished_at"] < value["started_at"]
        or value.get("status") not in ("running", "complete", "failed", "busy")
        or value.get("reason") not in REASONS
    ):
        return None
    result = {
        k: value[k] for k in ("mode", "started_at", "finished_at", "status", "reason")
    }
    if type(value.get("partial_possible")) is bool:
        result["partial_possible"] = value["partial_possible"]
    for k in COUNTS:
        if k in value:
            if type(value[k]) is not int or not 0 <= value[k] <= 2**53 - 1:
                return None
            result[k] = value[k]
    if complete:
        if value["status"] != "complete" or not all(k in result for k in COUNTS):
            return None
        if not isinstance(value.get("policy_digest"), str) or not re.fullmatch(
            r"[a-f0-9]{64}", value["policy_digest"]
        ):
            return None
        result["policy_digest"] = value["policy_digest"]
        counts = value.get("protection_counts")
        if not isinstance(counts, dict) or any(
            k not in PROTECTIONS or type(n) is not int or not 0 <= n <= 50000
            for k, n in counts.items()
        ):
            return None
        result["protection_counts"] = dict(counts)
        skips = value.get("skip_counts", {})
        if not isinstance(skips, dict) or any(
            k
            not in ("versions-limit", "objects-limit", "bytes-limit", "request-budget")
            or type(n) is not int
            or not 0 <= n <= 50000
            for k, n in skips.items()
        ):
            return None
        result["skip_counts"] = dict(skips)
    return result


def clean_file(value, key):
    if (
        not isinstance(value, dict)
        or value.get("schema_version") != 1
        or value.get("key") != key
    ):
        return {}
    result = {
        name: row
        for name in ("attempt", "last_plan", "last_preview", "last_apply")
        if (row := clean_record(value.get(name), name != "attempt")) is not None
    }
    fields = ("attempt", "last_plan", "last_preview", "last_apply")
    invalid = {name for name in fields if name in value and name not in result}
    old_invalid = value.get("invalid_fields", [])
    if isinstance(old_invalid, list):
        invalid.update(
            name for name in old_invalid if name in fields and name not in result
        )
    if invalid:
        result["invalid_fields"] = sorted(invalid)
    return result


def error_reason(error):
    if isinstance(error, TimeoutError):
        return "time-budget"
    message = str(error) if isinstance(error, ValueError) else ""
    if "exclusive-writer" in message:
        return "exclusive-writer-required"
    if "version" in message and "bucket" in message:
        return "versioned-bucket"
    if "limit" in message or "budget" in message:
        return "scan-limit"
    if any(
        word in message
        for word in ("changed", "conflict", "mismatch", "prefix not empty")
    ):
        return "changed-objects"
    response = getattr(error, "response", {})
    code = response.get("Error", {}).get("Code") if isinstance(response, dict) else None
    if code in ("AccessDenied", "InvalidAccessKeyId", "SignatureDoesNotMatch"):
        return "permission-denied"
    if code:
        return "storage-error"
    if isinstance(error, OSError):
        return "io-error"
    if isinstance(error, (ValueError, KeyError, TypeError)):
        return "unsafe-state"
    return "unknown-error"


def persist(config, key, record):
    """Merge independent modes and protect newer attempts from late completion."""
    from .__main__ import atomic

    if not isinstance(key, str) or not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", key):
        raise ValueError("invalid retention observation key")
    state = Path(config["state_root"])
    state.mkdir(parents=True, exist_ok=True)
    path = state / "object-retention-results" / (key + ".json")
    with (state / "object-retention-reports.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            previous = clean_file(read_json(path, 262144), key)
        except (OSError, ValueError, TypeError, RecursionError):
            previous = {}
        clean = clean_record(record, record["status"] == "complete")
        if clean is None:
            raise ValueError("invalid retention observation")
        if record["started_at"] >= previous.get("attempt", {}).get("started_at", 0):
            previous["attempt"] = clean_record(clean)
        if record["status"] == "complete":
            for field in ("last_plan", "last_" + record["mode"]):
                if record["finished_at"] >= previous.get(field, {}).get(
                    "finished_at", 0
                ):
                    previous[field] = clean
        if "invalid_fields" in previous:
            previous["invalid_fields"] = [
                name for name in previous["invalid_fields"] if name not in previous
            ]
            if not previous["invalid_fields"]:
                previous.pop("invalid_fields")
        atomic(path, dict(schema_version=1, key=key, **previous))


def observe(
    config, repositories, started, mode, status, reason, *, report=None, progress=None
):
    for repository in repositories:
        key = repository["key"]
        value = dict(
            mode=mode,
            started_at=started,
            finished_at=time.time(),
            status=status,
            reason=reason,
            partial_possible=mode == "apply" and status in ("running", "failed"),
        )
        if status == "complete":
            # Use the locked operation's completion, not a delayed report write.
            value["finished_at"] = report["completed_at"]
            row = next(r for r in report["repositories"] if r["key"] == key)
            candidates = row["candidates"]
            value.update(
                policy_digest=fingerprint(config, repository),
                candidate_versions=len(candidates),
                candidate_objects=sum(r["objects"] for r in candidates),
                candidate_bytes=sum(r["bytes"] for r in candidates),
                **{
                    k: row.get(k, 0)
                    for k in (
                        "eligible_versions",
                        "eligible_objects",
                        "eligible_bytes",
                        "skipped_versions",
                        "unknown_objects",
                        "removed_versions",
                        "removed_objects",
                        "removed_bytes",
                    )
                },
            )
            value["partial_possible"] = False
            counts = {}
            for reasons in row["protected"].values():
                for name in set(reasons):
                    counts[name] = counts.get(name, 0) + 1
            value["protection_counts"] = counts
            value["skip_counts"] = dict(row.get("skip_counts", {}))
            value["reason"] = (
                "preview-only"
                if mode == "preview"
                else "disabled"
                if not row["enabled"]
                else "budget-skipped"
                if row.get("skipped_versions", 0)
                else "applied"
                if row["removed_versions"]
                else "no-candidates"
            )
        elif progress and key in progress:
            value.update(progress[key])
        persist(config, key, value)


def summary(root, data_root, now=None):
    from .object_retention import policy
    from .retention import policy as local_policy
    from .object_info import scope

    now = time.time() if now is None else now
    try:
        config, keys, _ = scope(root / "private/repositories.yaml")
        if config.get("state_root") != "/state":
            raise ValueError("unsupported state mapping")
    except (OSError, ValueError, KeyError, TypeError, RecursionError, yaml.YAMLError):
        return dict(status="unavailable", repositories=[])
    rows = []
    for repository, key in zip(config["repositories"], keys):
        row = dict(key=key)
        try:
            settings = policy(config, repository)
            local = local_policy(config, repository)
            row["policy"] = {k: v for k, v in settings.items() if k != "pinned_shas"}
            row["policy"].update(
                pinned_count=len(
                    set(settings["pinned_shas"]) | set(local["pinned_shas"])
                ),
                rollback_versions=local["rollback_versions"],
            )
            digest = fingerprint(config, repository)
            path = data_root / "publisher/object-retention-results" / (key + ".json")
            if not path.exists() and not path.is_symlink():
                row["result_status"] = "never"
            else:
                source = read_json(path, 262144)
                records = clean_file(source, key)
                if not records:
                    raise ValueError("invalid object retention observations")
                row.update(records, result_status="ok")
                for field in ("last_plan", "last_preview", "last_apply"):
                    if field in row:
                        value = row[field]
                        value["age_seconds"] = max(0, round(now - value["finished_at"]))
                        value["policy_changed"] = value.pop("policy_digest") != digest
                        value["stale"] = (
                            value["age_seconds"] > STALE_SECONDS
                            or value["policy_changed"]
                        )
        except (
            OSError,
            ValueError,
            KeyError,
            TypeError,
            AttributeError,
            RecursionError,
        ):
            row["result_status"] = "unavailable"
        rows.append(row)
    return dict(status="ok", repositories=rows, stale_seconds=STALE_SECONDS)
