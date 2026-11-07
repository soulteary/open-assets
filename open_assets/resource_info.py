"""Scheduled bounded filesystem sampling; Info summary never scans directories."""

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import time
import stat
import yaml

from . import resources, resource_events
from .object_info import scope
from .storage_info import finite_time, read_json, write_cache

INTERVAL = 900
MAX_SECONDS = 60


def settings(root):
    from .telemetry import environment

    env = environment(root)
    interval = int(env.get("INFO_RESOURCE_INTERVAL_SECONDS", INTERVAL))
    warning = int(env.get("INFO_RESOURCE_WARN_PERCENT", 80))
    if not 60 <= interval <= 86400 or not 1 <= warning <= 99:
        raise ValueError("invalid resource observation settings")
    return interval, warning


def identity(config, keys):
    return hashlib.sha256(
        json.dumps([sorted(keys), resources.policy(config)], sort_keys=True).encode()
    ).hexdigest()


def clean_sample(value, keys):
    if not isinstance(value, dict) or not finite_time(value.get("measured_at")):
        raise ValueError("invalid resource sample")

    def number(v):
        return type(v) is int and 0 <= v < 2**53

    if not number(value.get("git_bytes")):
        raise ValueError("invalid Git total")
    rows = value.get("repositories")
    if (
        not isinstance(rows, dict)
        or set(rows) != set(keys)
        or any(not number(v) for v in rows.values())
    ):
        raise ValueError("invalid Git repository measurement")
    if sum(rows.values()) > value["git_bytes"]:
        raise ValueError("inconsistent Git total")
    queue = value.get("queue")
    if (
        not isinstance(queue, dict)
        or any(
            not number(queue.get(k))
            for k in ("active_records", "total_records", "reserved_bytes", "bytes")
        )
        or not isinstance(queue.get("locations"), dict)
        or set(queue["locations"]) != {"jobs", "job-archive", "quarantine"}
        or any(not number(v) for v in queue["locations"].values())
        or sum(queue["locations"].values()) != queue["total_records"]
        or queue["active_records"] != queue["locations"]["jobs"]
        or queue["reserved_bytes"] < max(queue["bytes"], queue["total_records"] * 65536)
    ):
        raise ValueError("invalid queue measurement")
    return dict(
        measured_at=value["measured_at"],
        git_bytes=value["git_bytes"],
        repositories=dict(rows),
        queue={
            k: queue[k]
            for k in (
                "active_records",
                "total_records",
                "reserved_bytes",
                "bytes",
                "locations",
            )
        },
    )


def collect(config, previous, now=None, force=False, interval=INTERVAL):
    now = time.time() if now is None else now
    keys = [r["key"] for r in config["repositories"]]
    expected = identity(config, keys)
    cache = dict(schema_version=1, scope=expected, status="never")
    if isinstance(previous, dict) and previous.get("scope") == expected:
        if previous.get("status") in ("ok", "busy", "unavailable"):
            cache["status"] = previous["status"]
        if finite_time(previous.get("attempted_at")):
            cache["attempted_at"] = previous["attempted_at"]
        try:
            cache["sample"] = clean_sample(previous["sample"], keys)
        except (KeyError, ValueError, TypeError):
            pass
    if (
        not force
        and cache["status"] != "busy"
        and 0 <= now - cache.get("attempted_at", 0) < interval
    ):
        return cache
    deadline = time.monotonic() + MAX_SECONDS

    def check():
        if time.monotonic() > deadline:
            raise TimeoutError("resource measurement timeout")

    try:
        state = Path(config["state_root"])
        # Observe a stable publication/queue boundary; do not wait for a writer.
        with contextlib.ExitStack() as stack:

            def acquire(name):
                fd = os.open(
                    state / (name + ".lock"),
                    os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                )
                stream = stack.enter_context(os.fdopen(fd, "r"))
                if not stat.S_ISREG(os.fstat(fd).st_mode):
                    raise ValueError("nonregular resource lock")
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)

            acquire("worker")
            p = resources.policy(config)
            git_root = state / "git"
            total, _ = resources.usage(git_root, p["scan_entries"], check)
            rows = {
                key: resources.usage(git_root / key, p["scan_entries"], check)[0]
                for key in keys
            }
            # Keep event admission available during the potentially larger Git scan.
            acquire("queue")
            queue = resources.queue_usage(config, check)
            sample = dict(
                measured_at=now, git_bytes=total, repositories=rows, queue=queue
            )
            cache.update(
                status="ok", attempted_at=now, sample=clean_sample(sample, keys)
            )
    except BlockingIOError:
        cache["status"] = "busy"  # Retry next timer tick, not after a full interval.
    except (OSError, ValueError, TypeError, resources.ResourceLimit):
        cache.update(status="unavailable", attempted_at=now)
    return cache


def summary(root, data_root, now=None):
    """Read a fixed cache and tiny fixed-slot event file, never enumerate state."""
    now = time.time() if now is None else now
    result = dict(
        status="unavailable",
        stale=True,
        repositories=[],
        alerts=[],
        events={},
        event_status="unavailable",
    )
    try:
        config, keys, _ = scope(root / "private/repositories.yaml")
        if config.get("state_root") != "/state":
            raise ValueError("unsupported resource state mapping")
        interval, warning = settings(root)
        p = resources.policy(config)
        try:
            raw_events = read_json(
                data_root / "publisher/resource-limit-events.json", 16384
            )
            result["events"] = resource_events.clean(raw_events, keys)
            if not isinstance(raw_events, dict) or set(raw_events) != set(
                result["events"]
            ):
                raise ValueError("invalid resource event observation")
            result["event_status"] = "ok"
        except FileNotFoundError:
            result.update(events={}, event_status="never")
        except (OSError, ValueError, TypeError):
            result.update(events={}, event_status="unavailable")
        cache = read_json(data_root / "info/resource-cache.json", 1048576)
        if cache.get("schema_version") != 1 or cache.get("scope") != identity(
            config, keys
        ):
            raise ValueError("resource scope changed")
        sample = clean_sample(cache["sample"], keys)
        status = cache.get("status")
        if status not in ("ok", "busy", "unavailable"):
            raise ValueError("invalid resource status")
        stale = (
            now - sample["measured_at"] > interval * 2
            or now < sample["measured_at"] - 5
        )
        result.update(
            status=status,
            stale=stale,
            interval_seconds=interval,
            warning_percent=warning,
            measured_at=sample["measured_at"],
            budget=p,
            queue=sample["queue"],
            git_bytes=sample["git_bytes"],
            repositories=[dict(key=k, bytes=sample["repositories"][k]) for k in keys],
        )
        alerts = []
        for code, used in (
            ("git_total_bytes", sample["git_bytes"]),
            ("queue_active_records", sample["queue"]["active_records"]),
            ("queue_total_records", sample["queue"]["total_records"]),
            ("queue_reserved_bytes", sample["queue"]["reserved_bytes"]),
        ):
            if used * 100 >= p[code] * warning:
                alerts.append(
                    dict(code=code, level="critical" if used >= p[code] else "warning")
                )
        for row in result["repositories"]:
            if row["bytes"] * 100 >= p["git_repository_bytes"] * warning:
                alerts.append(
                    dict(
                        code="git_repository_bytes",
                        key=row["key"],
                        level="critical"
                        if row["bytes"] >= p["git_repository_bytes"]
                        else "warning",
                    )
                )
        result["alerts"] = alerts
        return result
    except (
        OSError,
        ValueError,
        KeyError,
        TypeError,
        AttributeError,
        RecursionError,
        yaml.YAMLError,
    ):
        return result


def main():
    parser = argparse.ArgumentParser(
        description="Cache Git and queue resource measurements"
    )
    parser.add_argument("--root", type=Path, default=Path("/app/open-assets"))
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run the scheduled host collector as root")
    from .telemetry import environment

    root = args.root.resolve()
    config, _, _ = scope(root / "private/repositories.yaml")
    if config.get("state_root") != "/state":
        raise ValueError("unsupported state mapping")
    data = Path(environment(root)["DATA_ROOT"])
    config = dict(config, state_root=str(data / "publisher"))
    interval, _ = settings(root)
    path = data / "info/resource-cache.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    with (path.parent / ".resources.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            old = read_json(path, 1048576)
        except (OSError, ValueError):
            old = {}
        updated = collect(config, old, force=args.force, interval=interval)
        if updated != old:
            write_cache(path, updated)
    print("Resource measurement " + updated["status"])
    if updated["status"] == "unavailable":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
