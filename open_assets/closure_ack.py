"""Explicit acknowledgment of a specific closure refusal, not recovery closure."""

import hashlib
import json
import os
from pathlib import Path
import re
import stat
import time
import uuid

from . import __main__ as core, retention, restore_close_info
from .restore_close import save, write_once
from .restore_check import fingerprint, reference_fingerprint
from .storage_info import read_bytes, read_json

MAX_RECORDS = 1000
LIMITS = {
    "intent.json": 8192,
    "result.json": 8192,
    "source.json": 16384,
    "check.json": 16384,
}
GROUPS = ("restorations", "restoration_checks", "restoration_closures")


def valid_id(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{32}", value)


def directory(path, create=False):
    if create:
        path.mkdir(mode=0o700, exist_ok=True)
    if path.exists() or path.is_symlink():
        retention.real_directory(path)


def digest_file(path, limit):
    # Validate every parent below state_root before opening optional evidence.
    for parent in (path.parent.parent, path.parent):
        directory(parent)
    try:
        raw = read_bytes(path, limit)
    except FileNotFoundError:
        return "missing"
    return hashlib.sha256(raw).hexdigest()


def context(root, data, snapshot, evidence, key, audit_id):
    if not valid_id(audit_id):
        raise ValueError("invalid closure audit ID")
    rows = {
        name: next((r for r in snapshot[name]["repositories"] if r["key"] == key), None)
        for name in GROUPS
    }
    if any(
        snapshot[name].get("status") != "ok"
        or not row
        or row.get("result_status") == "unavailable"
        for name, row in rows.items()
    ):
        raise ValueError("current evidence unavailable")
    current = evidence["restoration_closures"].get(key, {})
    if (
        current.get("audit_id") != audit_id
        or rows["restoration_closures"].get("attempt", {}).get("status") != "refused"
    ):
        raise ValueError("only latest online closure refusal can be acknowledged")
    state = data / "publisher"
    directory(state)
    base = state / "restore-closures" / key / audit_id
    for parent in (base.parent.parent, base.parent, base):
        directory(parent)
    artifacts = {}
    for name, limit in LIMITS.items():
        try:
            artifacts[name] = read_bytes(base / name, limit)
        except FileNotFoundError:
            if name in ("intent.json", "result.json"):
                raise
    intent, result = (
        json.loads(artifacts[name]) for name in ("intent.json", "result.json")
    )
    from .restore_audit import validate

    validate(artifacts, key, audit_id)
    record = restore_close_info.clean(intent, result, key, audit_id)
    if record["status"] != "refused":
        raise ValueError("closure is not refused")
    sha = intent["sha"]
    config_digest = hashlib.sha256(
        read_bytes(root / "private/repositories.yaml", 1048576)
    ).hexdigest()
    host = dict(state_root=str(state), images_root=str(data / "images"))
    for parent in (
        data / "images",
        data / "images/repos",
        data / "images/repos" / key,
        state / "published",
    ):
        directory(parent)
    binding = dict(
        key=key,
        audit_id=audit_id,
        sha=sha,
        config_digest=config_digest,
        artifacts={
            name: hashlib.sha256(raw).hexdigest() for name, raw in artifacts.items()
        },
        source=digest_file(state / "object-restores" / key / (sha + ".json"), 16384),
        check=digest_file(state / "restore-checks" / key / (sha + ".json"), 16384),
        manifest=digest_file(state / "manifests" / key / (sha + ".json"), 4194304),
        references=reference_fingerprint(host, key),
        latest={name: evidence[name].get(key) for name in GROUPS},
    )
    return binding, artifacts


def observations(root, data):
    from . import restore_info, restore_check_info

    evidence = {name: {} for name in GROUPS}
    snapshot = {}
    for name, module in zip(
        GROUPS, (restore_info, restore_check_info, restore_close_info)
    ):
        snapshot[name] = module.summary(root, data, evidence=evidence[name])
    return snapshot, evidence


def location(data, key):
    base = data / "closure-acknowledgments"
    for parent in (data, base, base / key):
        directory(parent)
        if parent.exists():
            metadata = parent.lstat()
            if metadata.st_uid != os.getuid() or metadata.st_mode & 0o022:
                raise ValueError(
                    "unsafe acknowledgment parent ownership or permissions"
                )
    return base / key


def read_record(base, pointer):
    if pointer.get("schema_version") != 1 or not valid_id(pointer.get("ack_id")):
        raise ValueError("invalid acknowledgment pointer")
    folder = base / pointer["ack_id"]
    directory(folder)
    raw = read_bytes(folder / "ack.json", 16384)
    if hashlib.sha256(raw).hexdigest() != pointer.get("record_digest"):
        raise ValueError("acknowledgment integrity mismatch")
    value = json.loads(raw)
    if (
        value.get("schema_version") != 1
        or value.get("status") != "recorded"
        or value.get("ack_id") != pointer["ack_id"]
    ):
        raise ValueError("invalid acknowledgment record")
    reason = value.get("reason")
    if (
        not isinstance(reason, str)
        or not 1 <= len(reason.strip()) <= 256
        or any(ord(c) < 32 or ord(c) == 127 for c in reason)
    ):
        raise ValueError("invalid acknowledgment reason")
    if (
        type(value.get("acknowledged_at")) not in (int, float)
        or not 0 < value["acknowledged_at"] <= time.time() + 5
    ):
        raise ValueError("invalid acknowledgment time")
    if not isinstance(value.get("binding"), dict) or not isinstance(
        value["binding"].get("artifacts"), dict
    ):
        raise ValueError("invalid acknowledgment binding")
    for name, digest in value["binding"]["artifacts"].items():
        if (
            name not in LIMITS
            or hashlib.sha256(read_bytes(folder / name, LIMITS[name])).hexdigest()
            != digest
        ):
            raise ValueError("acknowledgment snapshot mismatch")
    if not {"intent.json", "result.json"}.issubset(value["binding"]["artifacts"]):
        raise ValueError("incomplete acknowledgment snapshots")
    if type(value.get("execution_uid")) is not int or value["execution_uid"] < 0:
        raise ValueError("invalid audit uid")
    return value


def summary(root, data, snapshot, evidence, row):
    """Fixed pointer and bounded files only; never enumerate acknowledgment history."""
    key = row["key"]
    try:
        base = location(data, key)
        try:
            pointer = read_json(base / "latest.json", 8192)
        except FileNotFoundError:
            return dict(status="none")
        value = read_record(base, pointer)
        current = evidence["restoration_closures"].get(key, {})
        if current.get("audit_id") != value["binding"].get("audit_id"):
            return dict(status="invalid", reason="new-attempt")
        binding, _ = context(root, data, snapshot, evidence, key, current["audit_id"])
        if binding != value["binding"]:
            return dict(status="invalid", reason="evidence-changed")
        return dict(status="valid", acknowledged_at=value["acknowledged_at"])
    except (OSError, ValueError, KeyError, TypeError, AttributeError, RecursionError):
        return dict(status="unavailable", reason="unreadable-evidence")


def run(root, key, audit_id, reason, confirm=None):
    if not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", key) or not valid_id(audit_id):
        raise ValueError("invalid acknowledgment identity")
    if (
        not isinstance(reason, str)
        or not 1 <= len(reason.strip()) <= 256
        or any(ord(c) < 32 or ord(c) == 127 for c in reason)
    ):
        raise ValueError("bounded acknowledgment reason required")
    if confirm is not None and (
        not isinstance(confirm, str) or not re.fullmatch(r"[0-9a-f]{64}", confirm)
    ):
        raise ValueError("invalid evidence token")
    from .telemetry import environment

    data = Path(environment(root)["DATA_ROOT"])
    config = core.load(root / "private/repositories.yaml")
    if config.get("state_root") != "/state" or config.get("images_root") != "/images":
        raise ValueError("unsupported state mapping")
    core.repo(config, key)
    config = dict(config, state_root=str(data / "publisher"))
    directory(data / "publisher")
    lock = data / "publisher/worker.lock"
    if not stat.S_ISREG(lock.lstat().st_mode):
        raise ValueError("invalid worker lock")
    try:
        with core.lock(config, blocking=False):
            snapshot, evidence = observations(root, data)
            binding, artifacts = context(root, data, snapshot, evidence, key, audit_id)
            token = fingerprint(binding)
            if confirm is None:
                return dict(
                    status="preview",
                    key=key,
                    audit_id=audit_id,
                    evidence_token=token,
                    hint="核实无需处理后，重复相同命令并追加 --confirm 本次evidence_token；不会替恢复结案。",
                )
            if confirm != token:
                return dict(
                    status="refused",
                    reason="evidence-changed",
                    hint="证据已变化，请重新核实并预览，不要复用旧令牌。",
                    key=key,
                    audit_id=audit_id,
                )
            base = location(data, key)
            for folder in (base.parent, base):
                directory(folder, create=True)
            pointer = base / "latest.json"
            if pointer.exists() or pointer.is_symlink():
                prior = read_record(base, read_json(pointer, 8192))
                if prior["binding"] == binding and prior["reason"] == reason.strip():
                    return dict(
                        status="acknowledged",
                        key=key,
                        audit_id=audit_id,
                        ack_id=prior["ack_id"],
                        reused=True,
                    )
            # Bound writes and fail closed on malformed history; no automatic deletion.
            deadline = time.monotonic() + 2
            with os.scandir(base) as entries:
                count = 0
                for entry in entries:
                    if entry.name == "latest.json":
                        continue
                    if not valid_id(entry.name) or not entry.is_dir(
                        follow_symlinks=False
                    ):
                        raise ValueError("unexpected acknowledgment entry")
                    count += 1
                    if count >= MAX_RECORDS or time.monotonic() > deadline:
                        raise ValueError("acknowledgment history limit")
            ack_id = uuid.uuid4().hex
            folder = base / ack_id
            folder.mkdir(mode=0o700)
            for name, raw in artifacts.items():
                write_once(folder / name, raw)
            value = dict(
                schema_version=1,
                status="recorded",
                ack_id=ack_id,
                acknowledged_at=time.time(),
                execution_uid=os.getuid(),
                reason=reason.strip(),
                binding=binding,
            )
            save(folder / "ack.json", value)
            # Re-observe before publishing the pointer, even for external writers not using the lock.
            current_snapshot, current_evidence = observations(root, data)
            current, _ = context(
                root, data, current_snapshot, current_evidence, key, audit_id
            )
            if current != binding:
                return dict(
                    status="refused",
                    reason="evidence-changed",
                    hint="证据已变化，请重新核实并预览，不要复用旧令牌。",
                    key=key,
                    audit_id=audit_id,
                    ack_id=ack_id,
                )
            core.atomic(
                pointer,
                dict(
                    schema_version=1,
                    ack_id=ack_id,
                    record_digest=hashlib.sha256(
                        read_bytes(folder / "ack.json", 16384)
                    ).hexdigest(),
                ),
            )
            return dict(
                status="acknowledged",
                key=key,
                audit_id=audit_id,
                ack_id=ack_id,
                reused=False,
            )
    except BlockingIOError:
        return dict(status="busy", key=key, audit_id=audit_id)
