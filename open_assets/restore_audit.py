"""Query and archive closure audits without discarding unverified evidence."""

import contextlib
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import tarfile
import time

from . import restore_close as close, restore_close_info as info, retention
from .restore_check import fingerprint
from .storage_info import finite_time, read_bytes

LIMITS = {
    "intent.json": 8192,
    "result.json": 8192,
    "source.json": 16384,
    "check.json": 16384,
}
MAX_ENTRIES = 20000
SCAN_SECONDS = 30
ARCHIVE_LIMIT = 262144
TAR_LIMIT = 131072


def directories(config, key):
    root = Path(config["state_root"])
    retention.real_directory(root)
    result = {
        name: root / folder / key
        for name, folder in (
            ("online", "restore-closures"),
            ("archive", "restore-audit-archives"),
            ("retired", "restore-audit-retired"),
        )
    }
    for path in result.values():
        for parent in (path.parent, path):
            if parent.exists() or parent.is_symlink():
                retention.real_directory(parent)
    return result


def validate(files, key, audit_id):
    if not set(files) <= set(LIMITS) or not {"intent.json", "result.json"} <= set(
        files
    ):
        raise ValueError("incomplete audit files")
    for name, raw in files.items():
        if not isinstance(raw, bytes) or len(raw) > LIMITS[name]:
            raise ValueError("audit file limit")
    intent = json.loads(files["intent.json"])
    result = json.loads(files["result.json"])
    record = info.clean(intent, result, key, audit_id)
    if record["status"] == "running":
        raise ValueError("unfinished audit")
    for name, field in (
        ("source.json", "source_report_digest"),
        ("check.json", "check_digest"),
    ):
        if name in files or result.get(field):
            if name not in files or hashlib.sha256(
                files[name]
            ).hexdigest() != result.get(field):
                raise ValueError("audit snapshot digest mismatch")
    if "source.json" in files:
        from .restore_info import clean as clean_source

        original = json.loads(files["source.json"])
        evidence = clean_source(original, key, intent["sha"], intent["scope_digest"])
        if (
            original.get("scope_digest") != intent["scope_digest"]
            or evidence["uncertain"] != result.get("source_uncertain")
            or original["status"] != result.get("source_status")
        ):
            raise ValueError("audit source binding mismatch")
    if "check.json" in files:
        from .restore_check_info import clean as clean_check

        checked = json.loads(files["check.json"])
        clean_check(checked, key, intent["sha"])
        if checked.get("source_report_digest") != result.get("source_report_digest"):
            raise ValueError("audit check binding mismatch")
    if result["status"] == "closed":
        info.verify_snapshots(files["source.json"], files["check.json"], intent, result)
    return dict(
        audit_id=audit_id,
        sha=intent["sha"],
        status=record["status"],
        started_at=intent["started_at"],
        finished_at=result["finished_at"],
        reason=result["reason"],
        source_uncertain=result.get("source_uncertain", False),
        source_digest=result.get("source_report_digest"),
        bytes=sum(map(len, files.values())),
        file_digest=fingerprint(
            {n: hashlib.sha256(b).hexdigest() for n, b in files.items()}
        ),
    )


def read_directory(path, partial=False):
    retention.real_directory(path)
    files = {}
    with os.scandir(path) as entries:
        for index, entry in enumerate(entries):
            if index >= len(LIMITS) or entry.name not in LIMITS:
                raise ValueError("unexpected audit entry")
            files[entry.name] = read_bytes(path / entry.name, LIMITS[entry.name])
    if not partial and "intent.json" not in files:
        raise ValueError("missing audit intent")
    return files


def archive_bytes(files, key, audit_id, sha):
    manifest = dict(
        schema_version=1,
        key=key,
        audit_id=audit_id,
        sha=sha,
        archived_at=time.time(),
        execution_uid=os.getuid(),
        files={
            n: dict(size=len(b), sha256=hashlib.sha256(b).hexdigest())
            for n, b in files.items()
        },
    )
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name, raw in sorted(
            dict(
                files,
                **{"manifest.json": json.dumps(manifest, sort_keys=True).encode()},
            ).items()
        ):
            member = tarfile.TarInfo(name)
            member.size = len(raw)
            member.mode = 0o600
            tar.addfile(member, io.BytesIO(raw))
    return gzip.compress(output.getvalue(), mtime=0)


def verify_archive(path, key, audit_id):
    raw = read_bytes(path, ARCHIVE_LIMIT)
    with gzip.GzipFile(fileobj=io.BytesIO(raw), mode="rb") as stream:
        decoded = stream.read(TAR_LIMIT + 1)
    if len(decoded) > TAR_LIMIT:
        raise ValueError("archive expansion limit")
    files = {}
    end = 0
    with tarfile.open(fileobj=io.BytesIO(decoded), mode="r:") as tar:
        for index, member in enumerate(tar):
            if (
                index >= 5
                or member.name in files
                or member.name not in (*LIMITS, "manifest.json")
                or not member.isfile()
                or member.type != tarfile.REGTYPE
                or member.size < 0
                or member.pax_headers
                or member.size > LIMITS.get(member.name, 8192)
            ):
                raise ValueError("unsafe archive member")
            files[member.name] = tar.extractfile(member).read()
            end = member.offset_data + ((member.size + 511) // 512) * 512
    if any(decoded[end:]):
        raise ValueError("unexpected archive trailer")
    manifest = json.loads(files.pop("manifest.json"))
    if (
        manifest.get("schema_version") != 1
        or manifest.get("key") != key
        or manifest.get("audit_id") != audit_id
        or not finite_time(manifest.get("archived_at"))
        or manifest.get("files")
        != {
            n: dict(size=len(b), sha256=hashlib.sha256(b).hexdigest())
            for n, b in files.items()
        }
    ):
        raise ValueError("archive manifest mismatch")
    record = validate(files, key, audit_id)
    if manifest.get("sha") != record["sha"]:
        raise ValueError("archive SHA mismatch")
    return record, files


def scan(config, key, deadline):
    paths = directories(config, key)
    records = {}
    count = 0
    for location, path in paths.items():
        if not path.exists():
            continue
        with os.scandir(path) as entries:
            for entry in entries:
                count += 1
                if count > MAX_ENTRIES:
                    raise ValueError("audit scan limit")
                retention.check_deadline(deadline)
                if location == "archive" and re.fullmatch(
                    r"tmp[a-z0-9_]{8}", entry.name
                ):
                    continue
                pattern = (
                    r"([0-9a-f]{32})\.tar\.gz"
                    if location == "archive"
                    else r"([0-9a-f]{32})"
                )
                match = re.fullmatch(pattern, entry.name)
                if not match:
                    raise ValueError("unexpected audit layout")
                audit_id = match[1]
                row = records.setdefault(
                    audit_id, dict(audit_id=audit_id, locations=[])
                )
                row["locations"].append(location)
                try:
                    if location == "archive":
                        record, _ = verify_archive(path / entry.name, key, audit_id)
                    elif location == "retired":
                        archive = paths["archive"] / (audit_id + ".tar.gz")
                        record, files = verify_archive(archive, key, audit_id)
                        remaining = read_directory(path / entry.name, partial=True)
                        if any(
                            name not in files or files[name] != raw
                            for name, raw in remaining.items()
                        ):
                            raise ValueError("retired audit conflicts with archive")
                    else:
                        files = read_directory(path / entry.name)
                        intent = json.loads(files["intent.json"])
                        info.header(intent, key, audit_id)
                        if "result.json" not in files:
                            record = dict(
                                audit_id=audit_id,
                                sha=intent["sha"],
                                status="running",
                                started_at=intent["started_at"],
                                bytes=sum(map(len, files.values())),
                            )
                        else:
                            record = validate(files, key, audit_id)
                    if "status" in row and (
                        row["status"] != record["status"]
                        or (
                            "file_digest" in row
                            and record.get("file_digest") != row["file_digest"]
                        )
                    ):
                        raise ValueError("archive/online audit conflict")
                    row.update(record)
                except (
                    OSError,
                    ValueError,
                    KeyError,
                    TypeError,
                    AttributeError,
                    RecursionError,
                    EOFError,
                    tarfile.TarError,
                ):
                    row["invalid"] = True
    return paths, list(records.values())


def public_row(row):
    return {
        k: v
        for k, v in row.items()
        if k
        in (
            "audit_id",
            "sha",
            "status",
            "started_at",
            "finished_at",
            "reason",
            "bytes",
            "locations",
            "invalid",
            "protection",
        )
    }


def plan(rows, sha, keep_last, min_age_days, max_records, now):
    online = [r for r in rows if "online" in r["locations"]]
    ordered = sorted(
        (r for r in online if not r.get("invalid")),
        key=lambda r: (r["started_at"], r["audit_id"]),
        reverse=True,
    )
    protected = {r["audit_id"] for r in ordered[:keep_last]}
    covered = {
        r.get("source_digest")
        for r in rows
        if not r.get("invalid") and r.get("status") == "closed"
    } - {None}
    candidates = []
    protections = []
    for row in sorted(rows, key=lambda r: (r.get("started_at", 0), r["audit_id"])):
        if sha is not None and row.get("sha") != sha and not row.get("invalid"):
            continue
        if not any(location in row["locations"] for location in ("online", "retired")):
            continue
        reason = (
            "invalid"
            if row.get("invalid")
            else "unfinished"
            if row.get("status") == "running"
            else "keep-last"
            if row["audit_id"] in protected
            else "unresolved-source"
            if row.get("source_uncertain")
            and row.get("status") != "closed"
            and row.get("source_digest") not in covered
            else "recent"
            if row.get("finished_at", now) > now - min_age_days * 86400
            else None
        )
        # Retired entries already passed selection before the atomic rename.
        if "retired" in row["locations"] and not row.get("invalid"):
            reason = None
        if reason:
            protections.append(dict(public_row(row), protection=reason))
            continue
        if len(candidates) < max_records:
            candidates.append(public_row(row))
        else:
            protections.append(dict(public_row(row), protection="batch-limit"))
    return candidates, protections, len(online)


def mkdir_private(path):
    if path.exists() or path.is_symlink():
        retention.real_directory(path)
    else:
        path.mkdir(mode=0o700)
        retention.sync_directory(path.parent)


def apply_one(paths, key, row, deadline):
    audit_id = row["audit_id"]
    online = paths["online"] / audit_id
    retired = paths["retired"] / audit_id
    archive = paths["archive"] / (audit_id + ".tar.gz")
    retention.check_deadline(deadline)
    if retired.exists() or retired.is_symlink():
        record, files = verify_archive(archive, key, audit_id)
    else:
        files = read_directory(online)
        record = validate(files, key, audit_id)
        if record["file_digest"] != row["file_digest"]:
            raise ValueError("audit changed since planning")
        if archive.exists() or archive.is_symlink():
            _, saved = verify_archive(archive, key, audit_id)
            if saved != files:
                raise ValueError("existing archive conflict")
        else:
            close.write_once(
                archive, archive_bytes(files, key, audit_id, record["sha"])
            )
        _, saved = verify_archive(archive, key, audit_id)
        if saved != files or read_directory(online) != files:
            raise ValueError("archive verification or source changed")
        os.rename(online, retired)
        retention.sync_directory(paths["online"])
        retention.sync_directory(paths["retired"])
    # Every remaining byte must match a fully validated archive before removal.
    _, saved = verify_archive(archive, key, audit_id)
    remaining = read_directory(retired, partial=True)
    if any(name not in saved or saved[name] != raw for name, raw in remaining.items()):
        raise ValueError("retired audit changed")
    for name in remaining:
        retention.check_deadline(deadline)
        (retired / name).unlink()
    retired.rmdir()
    retention.sync_directory(paths["retired"])


def run(
    config,
    key,
    sha=None,
    *,
    action="list",
    apply=False,
    keep_last=20,
    min_age_days=30,
    max_records=100,
    limit=100,
    offset=0,
):
    from . import __main__ as core

    if (
        not isinstance(key, str)
        or not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", key)
        or (sha is not None and not retention.valid_sha(sha))
    ):
        raise ValueError("invalid audit scope")
    if (
        action not in ("list", "archive", "verify")
        or type(apply) is not bool
        or (apply and action != "archive")
        or type(keep_last) is not int
        or not 1 <= keep_last <= 1000
        or type(min_age_days) is not int
        or not 0 <= min_age_days <= 36500
        or type(max_records) is not int
        or not 1 <= max_records <= 500
        or type(limit) is not int
        or not 1 <= limit <= 1000
        or type(offset) is not int
        or not 0 <= offset <= 1000000
    ):
        raise ValueError("invalid audit operation")
    # Disabled repositories retain queryable audit evidence.
    if key not in {r["key"] for r in config["repositories"]}:
        raise ValueError("unknown audit repository")
    deadline = time.monotonic() + SCAN_SECONDS
    with contextlib.ExitStack() as stack:
        try:
            stack.enter_context(core.lock(config, blocking=False))
        except BlockingIOError:
            return dict(status="busy", key=key)
        paths, rows = scan(config, key, deadline)
        if action in ("list", "verify"):
            selected = sorted(
                (
                    r
                    for r in rows
                    if sha is None or r.get("sha") == sha or r.get("invalid")
                ),
                key=lambda r: (r.get("started_at", 0), r["audit_id"]),
                reverse=True,
            )
            if action == "verify":
                selected = [
                    r
                    for r in selected
                    if any(
                        location in r["locations"]
                        for location in ("archive", "retired")
                    )
                ]
            return dict(
                status="failed"
                if any(r.get("invalid") for r in selected)
                else "complete",
                key=key,
                total=len(selected),
                offset=offset,
                has_more=len(selected) > offset + limit,
                records=[public_row(r) for r in selected[offset : offset + limit]],
            )
        candidates, protected, online_count = plan(
            rows, sha, keep_last, min_age_days, max_records, time.time()
        )
        result = dict(
            status="complete",
            key=key,
            mode="apply" if apply else "preview",
            online_records=online_count,
            candidates=candidates,
            protected=protected[:1000],
            protected_total=len(protected),
            protected_truncated=len(protected) > 1000,
            protection_counts={
                name: sum(r["protection"] == name for r in protected)
                for name in {r["protection"] for r in protected}
            },
            archived_records=0,
            partial_possible=False,
            info_limit=512,
            invalid_records=sum(bool(r.get("invalid")) for r in rows),
            candidate_bytes=sum(r.get("bytes", 0) for r in candidates),
            projected_online_records=online_count
            - sum("online" in r["locations"] for r in candidates),
        )
        if not apply:
            return result
        from .audit_observation import persist

        started = time.time()
        policy = dict(
            keep_last=keep_last, min_age_days=min_age_days, max_records=max_records
        )
        result["partial_possible"] = bool(candidates)
        persist(config, key, result, started, policy)
        try:
            for name in ("archive", "retired"):
                mkdir_private(paths[name].parent)
                mkdir_private(paths[name])
            for row in candidates:
                internal = next(r for r in rows if r["audit_id"] == row["audit_id"])
                result["partial_possible"] = True
                apply_one(paths, key, internal, deadline)
                result["archived_records"] += 1
                persist(config, key, result, started, policy)
            result["partial_possible"] = False
        except (Exception, KeyboardInterrupt) as error:
            from .restore_check import reason

            result.update(status="failed", failure_reason=reason(error))
        persist(config, key, result, started, policy, finished=True)
        return result
