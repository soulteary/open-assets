"""Low-frequency local audit measurement; ordinary Info reads only this cache."""

import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import time

import yaml

from . import audit_observation, restore_audit as audit, retention
from .object_info import scope
from .restore_check import fingerprint
from .storage_info import finite_time, read_json, write_cache

INTERVAL = 900
MAX_SECONDS = 90
LOCATIONS = ("online", "archive", "retired")
METRICS = ("records", "bytes", "disk_bytes")


def identity(keys):
    return fingerprint(["audit-cache-v1", sorted(keys)])


def number(value):
    return type(value) is int and 0 <= value <= 2**53 - 1


def clean_sample(value):
    if not isinstance(value, dict) or not finite_time(value.get("measured_at")):
        raise ValueError("invalid audit sample")
    result = dict(measured_at=value["measured_at"])
    for location in LOCATIONS:
        metrics = value.get(location)
        if not isinstance(metrics, dict) or any(
            not number(metrics.get(k)) for k in METRICS
        ):
            raise ValueError("invalid audit storage metric")
        result[location] = {k: metrics[k] for k in METRICS}
    for k in (
        "invalid_records",
        "candidate_records",
        "candidate_bytes",
        "eligible_records",
    ):
        if not number(value.get(k)):
            raise ValueError("invalid audit planning metric")
        result[k] = value[k]
    counts = value.get("protection_counts")
    if not isinstance(counts, dict) or any(
        k not in audit_observation.PROTECTIONS or not number(v)
        for k, v in counts.items()
    ):
        raise ValueError("invalid audit protections")
    result["protection_counts"] = dict(counts)
    return result


def sanitize(value, keys):
    result = dict(schema_version=1, scope=identity(keys), repositories={})
    if not isinstance(value, dict) or value.get("scope") != result["scope"]:
        return result
    rows = value.get("repositories", {})
    if not isinstance(rows, dict):
        return result
    for key in keys:
        old = rows.get(key, {})
        if not isinstance(old, dict):
            continue
        row = {}
        if old.get("status") in ("ok", "busy", "unavailable"):
            row["status"] = old["status"]
        if finite_time(old.get("attempted_at")):
            row["attempted_at"] = old["attempted_at"]
        if "sample" in old:
            try:
                row["sample"] = clean_sample(old["sample"])
            except (ValueError, TypeError, KeyError):
                row["status"] = "unavailable"
        result["repositories"][key] = row
    return result


def measure(paths, rows, deadline):
    result = {
        name: dict(
            records=sum(name in row["locations"] for row in rows), bytes=0, disk_bytes=0
        )
        for name in LOCATIONS
    }
    entries = 0
    for location, path in paths.items():
        if not path.exists():
            continue
        retention.real_directory(path)
        result[location]["disk_bytes"] += path.lstat().st_blocks * 512
        with os.scandir(path) as children:
            for child in children:
                entries += 1
                if entries > audit.MAX_ENTRIES * 6:
                    raise ValueError("audit measurement limit")
                retention.check_deadline(deadline)
                target = path / child.name
                metadata = target.lstat()
                result[location]["disk_bytes"] += metadata.st_blocks * 512
                if stat.S_ISREG(metadata.st_mode):
                    result[location]["bytes"] += metadata.st_size
                elif stat.S_ISDIR(metadata.st_mode) and location != "archive":
                    with os.scandir(target) as files:
                        for entry in files:
                            entries += 1
                            if entries > audit.MAX_ENTRIES * 6:
                                raise ValueError("audit measurement limit")
                            retention.check_deadline(deadline)
                            item = (target / entry.name).lstat()
                            if not stat.S_ISREG(item.st_mode):
                                raise ValueError("nonregular audit measurement entry")
                            result[location]["disk_bytes"] += item.st_blocks * 512
                            result[location]["bytes"] += item.st_size
                else:
                    raise ValueError("unsafe audit measurement entry")
    return result


def collect(config, previous, *, force=False, clock=time.time):
    from . import __main__ as core

    keys = [r["key"] for r in config["repositories"]]
    cache = sanitize(previous, keys)
    due = [
        key
        for key in keys
        if force
        or clock() - cache["repositories"].get(key, {}).get("attempted_at", 0)
        >= INTERVAL
    ]
    if not due:
        return cache
    with contextlib.ExitStack() as stack:
        try:
            stack.enter_context(core.lock(config, blocking=False))
        except BlockingIOError:
            for key in due:
                cache["repositories"].setdefault(key, {})["status"] = "busy"
            return cache
        deadline = time.monotonic() + MAX_SECONDS
        # Failed or large repositories do not monopolize first place each run.
        for key in sorted(
            due, key=lambda k: cache["repositories"].get(k, {}).get("attempted_at", 0)
        ):
            row = cache["repositories"].setdefault(key, {})
            if time.monotonic() >= deadline:
                row["status"] = "unavailable"
                continue
            row["attempted_at"] = clock()
            try:
                paths, rows = audit.scan(
                    config, key, min(deadline, time.monotonic() + audit.SCAN_SECONDS)
                )
                totals = measure(paths, rows, deadline)
                candidates, protected, _ = audit.plan(rows, None, 20, 30, 100, clock())
                counts = {
                    name: sum(r["protection"] == name for r in protected)
                    for name in audit_observation.PROTECTIONS
                }
                sample = dict(
                    measured_at=clock(),
                    invalid_records=sum(bool(r.get("invalid")) for r in rows),
                    candidate_records=len(candidates),
                    candidate_bytes=sum(r.get("bytes", 0) for r in candidates),
                    eligible_records=len(candidates) + counts["batch-limit"],
                    protection_counts=counts,
                    **totals,
                )
                row.update(status="ok", sample=clean_sample(sample))
            except (
                OSError,
                ValueError,
                KeyError,
                TypeError,
                AttributeError,
                RecursionError,
            ):
                row["status"] = "unavailable"
    return sanitize(cache, keys)


def summary(root, data_root, now=None):
    now = time.time() if now is None else now
    try:
        config, keys, _ = scope(root / "private/repositories.yaml")
        if config.get("state_root") != "/state":
            raise ValueError("unsupported audit state mapping")
        cache = sanitize(read_json(data_root / "info/audit-cache.json", 1048576), keys)
    except (
        OSError,
        ValueError,
        KeyError,
        TypeError,
        AttributeError,
        RecursionError,
        yaml.YAMLError,
    ):
        return dict(status="unavailable", repositories=[], interval_seconds=INTERVAL)
    rows = []
    host = dict(config, state_root=str(data_root / "publisher"))
    for key in keys:
        value = cache["repositories"].get(key, {})
        row = dict(key=key, status=value.get("status", "never"), stale=True)
        if "sample" in value:
            row.update(value["sample"])
            row["age_seconds"] = max(0, round(now - row["measured_at"]))
            row["stale"] = row["age_seconds"] > INTERVAL * 2
        try:
            row["archival"] = audit_observation.read(host, key)
            attempt = row["archival"].get("attempt", {})
            row["changed_since_sample"] = bool(
                row.get("measured_at")
                and attempt.get("finished_at", attempt.get("started_at", 0))
                > row["measured_at"]
            )
        except (
            OSError,
            ValueError,
            KeyError,
            TypeError,
            AttributeError,
            RecursionError,
        ):
            row["archival"] = dict(status="unavailable")
        rows.append(row)
    return dict(
        status="ok",
        repositories=rows,
        interval_seconds=INTERVAL,
        policy=dict(keep_last=20, min_age_days=30, max_records=100),
    )


def main():
    parser = argparse.ArgumentParser(
        description="Cached local restoration audit statistics"
    )
    parser.add_argument("--root", type=Path, default=Path("/app/open-assets"))
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args()
    if args.worker:
        from .__main__ import CONFIG

        config, keys, _ = scope(Path(CONFIG))
        raw = sys.stdin.read(1048577)
        if len(raw) > 1048576:
            raise ValueError("oversized audit cache")
        print(json.dumps(collect(config, json.loads(raw), force=args.force)))
        return
    if os.geteuid() != 0:
        parser.error("run the scheduled host collector as root")
    from .telemetry import environment

    root = args.root.resolve()
    directory = Path(environment(root)["DATA_ROOT"]) / "info"
    directory.mkdir(parents=True, exist_ok=True)
    os.umask(0o077)
    with (directory / ".audits.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        _, keys, _ = scope(root / "private/repositories.yaml")
        path = directory / "audit-cache.json"
        try:
            previous = sanitize(read_json(path, 1048576), keys)
        except (OSError, ValueError, TypeError):
            previous = {}
        command = [
            "docker",
            "compose",
            "--project-directory",
            str(root),
            "exec",
            "-T",
            "publisher",
            "python",
            "-m",
            "open_assets.audit_info",
            "--worker",
        ] + (["--force"] if args.force else [])
        try:
            result = subprocess.run(
                command,
                input=json.dumps(previous),
                capture_output=True,
                text=True,
                timeout=130,
            )
            if result.returncode or len(result.stdout) > 1048576:
                raise ValueError("audit worker failed")
            updated = json.loads(result.stdout)
            if updated.get("scope") != identity(keys):
                raise ValueError("audit scope changed")
            updated = sanitize(updated, keys)
        except (subprocess.SubprocessError, OSError, ValueError, AttributeError):
            updated = sanitize(previous, keys)
            for key in keys:
                updated["repositories"].setdefault(key, {})["status"] = "unavailable"
            write_cache(path, updated)
            raise SystemExit(
                "Audit collector failed; previous totals retained"
            ) from None
        if updated != previous:
            write_cache(path, updated)
        print("Filtered local audit statistics cache updated")
        if any(
            row.get("status") == "unavailable"
            for row in updated["repositories"].values()
        ):
            raise SystemExit(1)


if __name__ == "__main__":
    main()
