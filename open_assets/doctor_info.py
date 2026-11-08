"""Scheduled doctor cache; the web path only reads a filtered fixed file."""

import argparse
import copy
import fcntl
import hashlib
import math
import os
from pathlib import Path
import re
import time

from . import diagnostics
from .storage_info import read_bytes, read_json, write_cache

INTERVAL = 300
LIMIT = 1048576
REASONS = {
    "ok",
    "running",
    "not-enabled",
    "failed",
    "condition-skipped",
    "never-or-unknown",
    "overdue",
    "unavailable",
    "fresh",
    "never-or-invalid",
    "clock-skew",
    "stale",
    "collector-busy",
    "collector-scanning",
    "collector-never",
    "collector-unavailable",
    "unavailable-or-scope-changed",
}


def settings(root):
    from .telemetry import environment

    interval = int(environment(root).get("INFO_DOCTOR_INTERVAL_SECONDS", INTERVAL))
    if not 60 <= interval <= 86400:
        raise ValueError("invalid doctor interval")
    return interval


def identity(root):
    try:
        raw = read_bytes(root / "private/repositories.yaml", LIMIT)
    except (OSError, ValueError):
        raw = b"missing-or-unreadable"
    return hashlib.sha256(raw).hexdigest()


def number(value):
    if (
        type(value) not in (int, float)
        or not math.isfinite(value)
        or not 0 <= value <= 10**12
    ):
        raise ValueError("invalid diagnostic number")
    return value


def text(value):
    if (
        not isinstance(value, str)
        or not 0 < len(value) <= 1024
        or any(ord(c) < 32 for c in value)
    ):
        raise ValueError("invalid diagnostic text")
    return value


def report(value):
    """Discard unrecognized fields; no Docker/systemd output or exception text."""
    from .operator import SERVICES

    if not isinstance(value, dict) or value.get("status") not in ("ok", "attention"):
        raise ValueError("invalid doctor report")
    result = dict(status=value["status"])
    limits = dict(
        checks=16, timers=len(diagnostics.TIMERS), caches=401, components=len(SERVICES)
    )
    for group, limit in limits.items():
        rows = value.get(group)
        if not isinstance(rows, list) or len(rows) > limit:
            raise ValueError("invalid diagnostic rows")
        output = []
        for row in rows:
            clean = {}
            if group == "components":
                if row.get("service") not in SERVICES:
                    raise ValueError("unknown component")
                clean["service"] = row["service"]
                for field, allowed in (
                    (
                        "state",
                        {
                            "running",
                            "exited",
                            "created",
                            "restarting",
                            "paused",
                            "dead",
                            "removing",
                            "missing",
                            "unknown",
                        },
                    ),
                    ("health", {"healthy", "unhealthy", "starting", "unknown", ""}),
                ):
                    clean[field] = (
                        row.get(field) if row.get(field) in allowed else "unknown"
                    )
            else:
                allowed = (
                    ("ok", "attention", "optional")
                    if group == "timers"
                    else ("ok", "attention")
                )
                if row.get("status") not in allowed:
                    raise ValueError("invalid diagnostic status")
                clean.update(status=row["status"], hint=text(row["hint"]))
                if group == "checks":
                    if row.get("check") not in (
                        "configuration",
                        "containers",
                        "maintenance",
                        "timers",
                        "info-snapshot",
                        "caches",
                    ):
                        raise ValueError("unknown check")
                    clean["check"] = row["check"]
                else:
                    if row.get("reason") not in REASONS:
                        raise ValueError("unknown diagnostic reason")
                    clean["reason"] = row["reason"]
                if group == "timers":
                    name = row.get("timer")
                    if (
                        name not in diagnostics.TIMERS
                        or row.get("required") is not diagnostics.TIMERS[name][0]
                    ):
                        raise ValueError("unknown timer")
                    clean.update(timer=name, required=row["required"])
                    for key, choices in (
                        (
                            "active",
                            {
                                "active",
                                "inactive",
                                "failed",
                                "activating",
                                "deactivating",
                                "reloading",
                            },
                        ),
                        (
                            "enabled",
                            {
                                "enabled",
                                "enabled-runtime",
                                "disabled",
                                "static",
                                "masked",
                                "masked-runtime",
                                "linked",
                                "linked-runtime",
                                "indirect",
                                "alias",
                                "generated",
                                "transient",
                                "unknown",
                                "",
                            },
                        ),
                        (
                            "result",
                            {
                                "success",
                                "exit-code",
                                "signal",
                                "core-dump",
                                "timeout",
                                "watchdog",
                                "start-limit-hit",
                                "resources",
                                "protocol",
                                "oom-kill",
                                "exec-condition",
                                "unknown",
                            },
                        ),
                    ):
                        if key in row:
                            clean[key] = row[key] if row[key] in choices else "unknown"
                    if "exit_code" in row:
                        clean["exit_code"] = number(row["exit_code"])
                if group == "caches":
                    if row.get("cache") not in (
                        "local-versions",
                        "objects",
                        "audits",
                        "resources",
                    ):
                        raise ValueError("unknown cache")
                    clean["cache"] = row["cache"]
                    key = row.get("key")
                    if key is not None:
                        if not isinstance(key, str) or not re.fullmatch(
                            r"[a-z][a-z0-9-]{0,47}", key
                        ):
                            raise ValueError("invalid repository key")
                        clean["key"] = key
                for key in ("age_seconds", "max_age_seconds"):
                    if row.get(key) is not None:
                        clean[key] = number(row[key])
            output.append(clean)
        result[group] = output
    if {r["timer"] for r in result["timers"]} != set(diagnostics.TIMERS):
        raise ValueError("incomplete timer report")
    if not {
        "configuration",
        "containers",
        "timers",
        "info-snapshot",
        "caches",
    }.issubset({r["check"] for r in result["checks"]}):
        raise ValueError("incomplete checks")
    return result


def previous(root, path):
    try:
        value = read_json(path, LIMIT)
        if value.get("schema_version") != 1 or value.get("scope") != identity(root):
            raise ValueError("diagnostic scope changed")
        result = dict(schema_version=1, scope=value["scope"])
        if value.get("status") not in ("ok", "running", "unavailable"):
            raise ValueError("invalid collection status")
        result["status"] = value["status"]
        for key in ("attempted_at", "measured_at"):
            if key in value:
                result[key] = number(value[key])
        if "report" in value:
            result["report"] = report(value["report"])
        return result
    except (OSError, ValueError, TypeError, KeyError, AttributeError, RecursionError):
        return dict(schema_version=1, scope=identity(root), status="unavailable")


def collect(root, path, force=False, now=None):
    from .operator import doctor

    fixed_now = now
    now = time.time() if now is None else now
    interval = settings(root)
    cache = previous(root, path)
    if (
        not force
        and cache["status"] != "running"
        and 0 <= now - cache.get("attempted_at", 0) < interval
    ):
        return cache
    cache.update(status="running", attempted_at=now)
    # An interrupted run is visible even if systemd terminates the process.
    write_cache(path, cache)
    try:
        sample = report(doctor(root))
        cache.update(
            status="ok",
            measured_at=time.time() if fixed_now is None else now,
            report=sample,
        )
    except Exception:
        # Do not persist exception text: it may include private runtime state.
        cache["status"] = "unavailable"
    write_cache(path, cache)
    return cache


def summary(root, data, now=None):
    now = time.time() if now is None else now
    try:
        interval = settings(root)
        value = previous(root, data / "info/doctor-cache.json")
        measured = value.get("measured_at")
        valid = measured is not None and "report" in value
        age = max(0, round(now - measured)) if valid else None
        stale = not valid or measured > now + 5 or age > interval * 2
        result = dict(
            status=value["status"],
            stale=stale,
            interval_seconds=interval,
            age_seconds=age,
            measured_at=measured,
            attempted_at=value.get("attempted_at"),
        )
        if valid:
            result["report"] = copy.deepcopy(value["report"])
        return result
    except (OSError, ValueError, TypeError, KeyError):
        return dict(status="unavailable", stale=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description="Cache read-only doctor diagnostics")
    parser.add_argument("--root", type=Path, default=Path("/app/open-assets"))
    parser.add_argument(
        "--force",
        action="store_true",
        help="ignore sample interval, not the collector lock",
    )
    args = parser.parse_args(argv)
    if os.geteuid() != 0:
        parser.error("run the host collector as root")
    from .telemetry import environment

    root = args.root.resolve()
    directory = Path(environment(root)["DATA_ROOT"]) / "info"
    directory.mkdir(parents=True, exist_ok=True)
    with (directory / ".doctor-collector.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("Doctor collector busy; cache unchanged")
            return 0
        value = collect(root, directory / "doctor-cache.json", args.force)
    print("Doctor collection " + value["status"])
    return 1 if value["status"] == "unavailable" else 0


if __name__ == "__main__":
    raise SystemExit(main())
