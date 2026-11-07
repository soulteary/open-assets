"""One bounded observation per resource category, serialized by queue.lock."""

from pathlib import Path
import time

from .resources import DEFAULTS, ResourceLimit
from .storage_info import finite_time, read_json

CODES = set(DEFAULTS) | {"unsafe_resource_entry"}


def clean(value, keys):
    result = {}
    if not isinstance(value, dict):
        return result
    for category in ("queue", "git"):
        event = value.get(category)
        if (
            isinstance(event, dict)
            and event.get("code") in CODES
            and finite_time(event.get("at"))
            and (event.get("key") in keys or event.get("key") is None)
            and (not event["code"].startswith("queue_") or category == "queue")
            and (not event["code"].startswith("git_") or category == "git")
        ):
            result[category] = dict(
                code=event["code"], at=event["at"], key=event.get("key")
            )
    return result


def record(config, error, key, category=None):
    """Best effort only: failed observation writes must not mask refusal.

    Caller holds queue.lock; no payload, URL, credential, or exception text is
    persisted. Two fixed slots keep this file from becoming another history.
    """
    if not isinstance(error, ResourceLimit) or str(error) not in CODES:
        return
    from .__main__ import atomic

    keys = [r["key"] for r in config["repositories"]]
    path = Path(config["state_root"]) / "resource-limit-events.json"
    try:
        try:
            value = clean(read_json(path, 16384), keys)
        except (OSError, ValueError, TypeError, KeyError, RecursionError):
            value = {}
        category = category or ("queue" if str(error).startswith("queue_") else "git")
        value[category] = dict(
            code=str(error), at=time.time(), key=key if key in keys else None
        )
        atomic(path, value)
    except (OSError, ValueError, TypeError, KeyError, RecursionError):
        pass
