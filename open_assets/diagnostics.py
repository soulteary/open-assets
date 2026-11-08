"""Read-only timer and bounded cache diagnostics for the host CLI."""

import hashlib
import math
import subprocess
import time

from . import audit_info, object_info, resource_info, storage_info

# Maximum age of a completed run, including normal interval and scheduling slack.
TIMERS = {
    "info": (True, 120),
    "objects": (True, 2100),
    "audits": (True, 2100),
    "resources": (True, 180),
    "logrotate": (True, 7800),
    "repair": (False, 180),
    "retention": (False, 46800),
    "object-retention": (False, 46800),
}
PROPERTIES = (
    "Id,LoadState,ActiveState,UnitFileState,Result,ExecMainCode,ExecMainStatus,"
    "ExecMainStartTimestampMonotonic,ExecMainExitTimestampMonotonic,"
    "ConditionResult,ConditionTimestampMonotonic"
)


def timer(root, name, inspect, uptime):
    required, max_age = TIMERS[name]
    unit = "open-assets-" + name
    row = dict(
        timer=name,
        required=required,
        status="attention",
        reason="unavailable",
        max_age_seconds=max_age,
    )
    unavailable_hint = (
        "检查 sudo systemctl status "
        + unit
        + ".timer "
        + unit
        + ".service --no-pager；确认单元已安装且当前用户可读取。"
    )
    row["hint"] = unavailable_hint
    try:
        raw = inspect(
            [
                "systemctl",
                "show",
                unit + ".timer",
                unit + ".service",
                "--property=" + PROPERTIES,
            ]
        )
        blocks = [
            dict(line.split("=", 1) for line in block.splitlines() if "=" in line)
            for block in raw.strip().split("\n\n")
        ]
        units = {block["Id"]: block for block in blocks}
        schedule, service = units[unit + ".timer"], units[unit + ".service"]
        active = schedule["ActiveState"]
        row.update(active=active, enabled=schedule.get("UnitFileState", "unknown"))
        if schedule.get("LoadState") == "not-found" or active != "active":
            row.update(
                reason="not-enabled", status="attention" if required else "optional"
            )
            row["hint"] = (
                "安装单元后执行 sudo systemctl enable --now " + unit + ".timer。"
                if required
                else "未启用可选功能；无需为通过检查而开启。启用前阅读对应运维文档。"
            )
            # A disabled timer must not conceal a recorded service failure.
            if (
                service.get("Result", "success") not in ("success", "")
                or service.get("ActiveState") == "failed"
                or int(service.get("ExecMainStatus", "0")) != 0
                or int(service.get("ExecMainCode", "0")) not in (0, 1)
            ):
                row.update(status="attention", reason="failed")
            else:
                return row
        elif service.get("LoadState") != "loaded":
            return row
        else:
            row.update(reason="ok", status="ok")
        result = service.get("Result", "unknown")
        code = int(service.get("ExecMainCode", "0"))
        exit_status = int(service.get("ExecMainStatus", "0"))
        started = int(service.get("ExecMainStartTimestampMonotonic", "0")) / 1e6
        finished = int(service.get("ExecMainExitTimestampMonotonic", "0")) / 1e6
        row.update(result=result, exit_code=exit_status)
        if service.get("ActiveState") in ("activating", "active", "deactivating"):
            row.update(status="ok", reason="running")
            row["hint"] = "服务正在执行；完成后重新运行 doctor 检查结果。"
            return row
        if result != "success" or code not in (0, 1) or exit_status != 0:
            row.update(status="attention", reason="failed")
        elif (
            service.get("ConditionResult") == "no"
            and int(service.get("ConditionTimestampMonotonic", "0")) > 0
        ):
            row.update(status="attention", reason="condition-skipped")
        elif not started or not finished or finished < started or finished > uptime + 5:
            row.update(status="attention", reason="never-or-unknown")
        else:
            row["age_seconds"] = max(0, round(uptime - finished))
            if row["age_seconds"] > max_age:
                row.update(status="attention", reason="overdue")
        if row["status"] == "attention":
            row["hint"] = (
                "查看 sudo journalctl -u " + unit + ".service -n 50 --no-pager；"
                "检查 private/maintenance.flag 与失败原因。"
                + (
                    "修复后可执行 sudo systemctl start "
                    + unit
                    + ".service，再运行 doctor。"
                    if name in ("info", "objects", "audits", "resources", "logrotate")
                    else "此服务可能修复容器或清理数据；先复核策略，不要直接补跑。"
                )
            )
        elif row["reason"] == "ok":
            row["hint"] = "调度已启用，最近执行成功。"
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        row.update(status="attention", reason="unavailable", hint=unavailable_hint)
    return row


def local_summary(root, data, config):
    """Only read the fixed cache; do not call collect_storage (which scans/writes)."""
    cache = storage_info.read_json(data / "info/repositories-cache.json")
    digest = hashlib.sha256(
        storage_info.read_bytes(root / "private/repositories.yaml", 1048576)
    ).hexdigest()
    if cache.get("schema_version") != 1 or cache.get("config_digest") != digest:
        raise ValueError("cache scope changed")
    return dict(
        status="ok",
        interval_seconds=storage_info.CACHE_SECONDS,
        repositories=[
            dict(
                key=r["key"],
                status="unavailable" if r["key"] in cache.get("failures", []) else "ok",
                **(
                    storage_info.cached_metrics(cache["repositories"].get(r["key"]))
                    or {}
                ),
            )
            for r in config["repositories"]
        ],
    )


def caches(root, data, config, now=None):
    now = time.time() if now is None else now
    rows = []
    sources = (
        ("local-versions", "info", lambda: local_summary(root, data, config)),
        ("objects", "objects", lambda: object_info.summary(root, data, now)),
        ("audits", "audits", lambda: audit_info.summary(root, data, now)),
        ("resources", "resources", lambda: resource_info.summary(root, data, now)),
    )
    for name, service, read in sources:
        hint = (
            "检查 open-assets-"
            + service
            + ".service 的日志；修复后可执行 sudo systemctl start open-assets-"
            + service
            + ".service，再运行 doctor。"
        )
        try:
            summary = read()
            samples = (
                [summary] if name == "resources" else summary.get("repositories", [])
            )
            expected = {r["key"] for r in config["repositories"]}
            if name != "resources" and {r["key"] for r in samples} != expected:
                raise ValueError("missing repository samples")
            if name != "resources" and summary.get("status") != "ok":
                raise ValueError("collector unavailable or busy")
            interval = summary["interval_seconds"]
            for sample in samples:
                measured = sample.get("measured_at")
                age = None
                reason = "fresh"
                if (
                    type(measured) not in (float, int)
                    or not math.isfinite(measured)
                    or measured <= 0
                ):
                    reason = "never-or-invalid"
                elif measured > now + 5:
                    reason = "clock-skew"
                else:
                    age = max(0, round(now - measured))
                    if age > interval * 2:
                        reason = "stale"
                if sample.get("status", "ok") != "ok":
                    reason = (
                        "collector-" + str(sample.get("status"))
                        if sample.get("status")
                        in ("busy", "scanning", "never", "unavailable")
                        else "collector-unavailable"
                    )
                rows.append(
                    dict(
                        cache=name,
                        key=sample.get("key"),
                        status="ok" if reason == "fresh" else "attention",
                        reason=reason,
                        age_seconds=age,
                        max_age_seconds=interval * 2,
                        hint="采样仍在有效期内。" if reason == "fresh" else hint,
                    )
                )
        except (
            OSError,
            ValueError,
            TypeError,
            KeyError,
            AttributeError,
            RecursionError,
        ):
            rows.append(
                dict(
                    cache=name,
                    status="attention",
                    reason="unavailable-or-scope-changed",
                    hint=hint,
                )
            )
    return rows
