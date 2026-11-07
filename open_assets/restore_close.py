"""Explicit closure records with immutable source/check evidence; no data repair."""

import contextlib
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
import time
import uuid

from . import object_restore as restore, restore_check as check, retention
from .object_retention import bucket_scope
from .storage_info import read_bytes

REASONS = {
    "verified",
    "source-unavailable",
    "ineligible",
    "check-failed",
    "references-missing",
    "evidence-changed",
    "io",
    "permission",
    "timeout",
    "interrupted",
    "unavailable",
    "missing",
    "mismatch",
    "storage",
}


class Refused(Exception):
    pass


def write_once(path, raw):
    """Publish a synced file without replacing an existing audit entry."""
    fd, temporary = tempfile.mkstemp(dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.link(temporary, path, follow_symlinks=False)
        retention.sync_directory(path.parent)
    finally:
        os.unlink(temporary)
        retention.sync_directory(path.parent)


def save(path, value):
    write_once(path, json.dumps(value, sort_keys=True).encode())


def run(config, key, sha, reason, s3=None):
    from . import __main__ as core

    if (
        not isinstance(key, str)
        or not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", key)
        or not retention.valid_sha(sha)
    ):
        raise ValueError("invalid closure identity")
    if (
        not isinstance(reason, str)
        or not 1 <= len(reason.strip()) <= 256
        or any(ord(c) < 32 or ord(c) == 127 for c in reason)
    ):
        raise ValueError("closure requires a bounded audit reason")
    core.repo(config, key)
    budget = restore.Budget()
    with contextlib.ExitStack() as stack:
        try:
            stack.enter_context(core.lock(config, blocking=False))
        except BlockingIOError:
            return dict(status="busy", key=key, sha=sha)
        root = Path(config["state_root"])
        retention.real_directory(root)
        parent = root / "restore-closures" / key
        restore.directories(parent.parent, parent)
        audit_id = uuid.uuid4().hex
        directory = parent / audit_id
        directory.mkdir(mode=0o700)
        retention.sync_directory(parent)
        intent = dict(
            schema_version=1,
            key=key,
            sha=sha,
            audit_id=audit_id,
            status="running",
            started_at=time.time(),
            scope_digest=bucket_scope(config),
            reason=reason.strip(),
            execution_uid=os.getuid(),
        )
        save(directory / "intent.json", intent)
        result = {
            k: intent[k]
            for k in (
                "schema_version",
                "key",
                "sha",
                "audit_id",
                "started_at",
                "scope_digest",
            )
        }
        result.update(status="failed", reason="unavailable")
        try:
            budget.check()
            try:
                digest, source_status, evidence = check.source(config, key, sha)
            except Exception:
                raise Refused("source-unavailable") from None
            if digest is None:
                raise Refused("source-unavailable")
            raw = read_bytes(restore.report_path(config, key, sha), 16384)
            if hashlib.sha256(raw).hexdigest() != digest:
                raise Refused("evidence-changed")
            write_once(directory / "source.json", raw)
            result.update(
                source_report_digest=digest,
                source_status=source_status,
                source_uncertain=evidence["uncertain"],
            )
            if not evidence["uncertain"]:
                raise Refused("ineligible")
            checked = check.run_locked(config, key, sha, s3, budget)
            check_raw = json.dumps(checked, sort_keys=True).encode()
            write_once(directory / "check.json", check_raw)
            result.update(
                check_digest=hashlib.sha256(check_raw).hexdigest(),
                manifest_digest=checked.get("manifest_digest"),
                reference_digest=checked.get("reference_digest"),
            )
            if (
                checked.get("status") != "complete"
                or checked.get("diagnosis") != "verified"
            ):
                raise Refused("check-failed")
            if checked["checks"]["references"]["reason"] not in (
                "target-current",
                "other-current",
            ):
                raise Refused("references-missing")
            budget.check()
            current_digest, _, _ = check.source(config, key, sha)
            manifest = retention.read_json(root / "manifests" / key / (sha + ".json"))
            if (
                current_digest != digest
                or checked.get("source_report_digest") != digest
                or check.fingerprint(manifest) != checked.get("manifest_digest")
                or check.reference_fingerprint(config, key)
                != checked.get("reference_digest")
            ):
                raise Refused("evidence-changed")
            if check.barrier(config, key, sha) not in ("none", "restored"):
                raise Refused("evidence-changed")
            result.update(
                status="closed",
                reason="verified",
                current_is_target=checked["current_is_target"],
                published_is_target=checked["published_is_target"],
            )
        except Refused as error:
            result.update(status="refused", reason=str(error))
        except (Exception, KeyboardInterrupt) as error:
            result.update(status="failed", reason=check.reason(error))
        result["finished_at"] = time.time()
        save(directory / "result.json", result)
        return result
