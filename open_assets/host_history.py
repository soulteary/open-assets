"""Bounded host-only numeric history; no container environment or credentials."""

import json
import math
import os
import stat

INTERVAL = 30
RETENTION = 86400
MAX_POINTS = 2881
MAX_BYTES = 1024**2


def number(value, maximum):
    return (
        type(value) in (int, float) and math.isfinite(value) and 0 <= value <= maximum
    )


def point(host, timestamp):
    if not number(timestamp, 2**53):
        raise ValueError("invalid history time")
    memory = host.get("memory", {})
    used, total = memory.get("used"), memory.get("total")
    load = host.get("load", [])
    return dict(
        at=timestamp,
        cpu_percent=host.get("cpu_percent")
        if number(host.get("cpu_percent"), 100)
        else None,
        memory_used=used
        if number(used, 2**53) and number(total, 2**53) and 0 < total and used <= total
        else None,
        memory_total=total if number(total, 2**53) and total > 0 else None,
        load=[
            value if number(value, 1e6) else None
            for value in (list(load)[:3] + [None] * 3)[:3]
        ]
        if isinstance(load, list)
        else [None] * 3,
        cpu_count=host.get("cpu_count")
        if type(host.get("cpu_count")) is int and 1 <= host["cpu_count"] <= 65536
        else None,
    )


def update(path, host, timestamp):
    points = []
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_BYTES:
                raise ValueError("unsafe history")
            old = json.loads(stream.read(MAX_BYTES + 1))
        if old.get("schema_version") != 1 or not isinstance(old.get("points"), list):
            raise ValueError("unsupported history")
        for value in old["points"][-MAX_POINTS:]:
            if not isinstance(value, dict) or not number(value.get("at"), timestamp):
                continue
            if value["at"] < timestamp - RETENTION:
                continue
            # Reconstruct an explicit numeric allowlist, even after file damage.
            points.append(
                point(
                    dict(
                        cpu_percent=value.get("cpu_percent"),
                        cpu_count=value.get("cpu_count"),
                        memory=dict(
                            used=value.get("memory_used"),
                            total=value.get("memory_total"),
                        ),
                        load=value.get("load"),
                    ),
                    value["at"],
                )
            )
    except (OSError, ValueError, TypeError, AttributeError):
        points = []
    points.append(point(host, timestamp))
    buckets = {
        int(p["at"] // INTERVAL): p for p in sorted(points, key=lambda p: p["at"])
    }
    result = dict(
        schema_version=1,
        interval_seconds=INTERVAL,
        retention_seconds=RETENTION,
        points=list(buckets.values())[-MAX_POINTS:],
    )
    return result
