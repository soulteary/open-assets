"""Bounded apply-only archival outcomes; preview remains read-only."""

from pathlib import Path

from . import retention
from .storage_info import finite_time

PROTECTIONS = {
    "invalid",
    "unfinished",
    "keep-last",
    "unresolved-source",
    "recent",
    "batch-limit",
}
REASONS = {
    "missing",
    "permission",
    "timeout",
    "mismatch",
    "io",
    "storage",
    "interrupted",
    "unavailable",
}


def clean(value):
    if (
        not isinstance(value, dict)
        or value.get("status") not in ("running", "complete", "failed")
        or not finite_time(value.get("started_at"))
    ):
        raise ValueError("invalid archival observation")
    result = {k: value[k] for k in ("status", "started_at")}
    if value["status"] != "running":
        if (
            not finite_time(value.get("finished_at"))
            or value["finished_at"] < value["started_at"]
        ):
            raise ValueError("invalid archival time")
        result["finished_at"] = value["finished_at"]
    for field in (
        "candidate_records",
        "archived_records",
        "protected_records",
        "keep_last",
        "min_age_days",
        "max_records",
    ):
        if type(value.get(field)) is not int or not 0 <= value[field] <= (
            36500 if field == "min_age_days" else 20000
        ):
            raise ValueError("invalid archival counter")
        result[field] = value[field]
    if type(value.get("partial_possible")) is not bool:
        raise ValueError("invalid archival intent")
    result["partial_possible"] = value["partial_possible"]
    counts = value.get("protection_counts")
    if not isinstance(counts, dict) or any(
        k not in PROTECTIONS or type(v) is not int or not 0 <= v <= 20000
        for k, v in counts.items()
    ):
        raise ValueError("invalid archival protections")
    result["protection_counts"] = dict(counts)
    if value.get("failure_reason") in REASONS:
        result["failure_reason"] = value["failure_reason"]
    return result


def read(config, key):
    path = Path(config["state_root"]) / "audit-archive-results" / (key + ".json")
    if not path.exists() and not path.is_symlink():
        return dict(status="never")
    retention.real_directory(path.parent)
    value = retention.read_json(path, 16384)
    if value.get("schema_version") != 1 or value.get("key") != key:
        raise ValueError("invalid archival observation identity")
    result = dict(status="ok", attempt=clean(value["attempt"]))
    if "last_complete" in value:
        result["last_complete"] = clean(value["last_complete"])
        if result["last_complete"]["status"] != "complete":
            raise ValueError("invalid archival complete history")
    return result


def persist(config, key, report, started, policy, *, finished=False):
    from . import __main__ as core
    import time

    path = Path(config["state_root"]) / "audit-archive-results" / (key + ".json")
    if path.parent.exists() or path.parent.is_symlink():
        retention.real_directory(path.parent)
    else:
        path.parent.mkdir(mode=0o700)
        retention.sync_directory(path.parent.parent)
    previous = read(config, key)
    value = dict(
        status=report["status"] if finished else "running",
        started_at=started,
        candidate_records=len(report["candidates"]),
        archived_records=report["archived_records"],
        protected_records=report["protected_total"],
        protection_counts=report["protection_counts"],
        partial_possible=report["partial_possible"],
        **policy,
    )
    if finished:
        value["finished_at"] = time.time()
    if report.get("failure_reason") in REASONS:
        value["failure_reason"] = report["failure_reason"]
    output = dict(schema_version=1, key=key, attempt=clean(value))
    if previous.get("last_complete"):
        output["last_complete"] = previous["last_complete"]
    if finished and report["status"] == "complete":
        output["last_complete"] = clean(value)
    core.atomic(path, output)
