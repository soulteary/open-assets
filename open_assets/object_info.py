"""Low-frequency S3 listing, private resumable cache and filtered Info summaries.

The root host command invokes the worker inside Publisher to reuse its existing
ListBucket permission and TLS trust. The worker only calls ListObjectsV2; neither
the HTTP service nor the 30-second host collector makes S3 requests.
"""

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

import yaml

from .storage_info import finite_time, read_bytes, read_json, write_cache

INTERVAL = 900
MAX_PAGES = 100
MAX_SECONDS = 110
MAX_KEYS = 1000
MAX_SCAN_AGE = 86400
MAX_INTEGER = 2**53 - 1


def integer(value):
    return type(value) is int and 0 <= value <= MAX_INTEGER


def scope(path):
    raw = read_bytes(path, 1048576)
    config = yaml.safe_load(raw)
    if (
        not isinstance(config, dict)
        or config.get("schema_version") != 1
        or not isinstance(config.get("repositories"), list)
        or not 0 < len(config["repositories"]) <= 100
        or not isinstance(config.get("bucket"), str)
        or not re.fullmatch(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]", config["bucket"])
        or not isinstance(config.get("endpoint"), str)
        or not config["endpoint"].startswith(("https://", "http://"))
    ):
        raise ValueError("invalid object statistics scope")
    keys = [r["key"] for r in config["repositories"]]
    if any(
        not isinstance(k, str) or not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", k)
        for k in keys
    ) or len(set(keys)) != len(keys):
        raise ValueError("invalid object statistics keys")
    # Only S3 identity/scope changes invalidate the object cache, not retention.
    digest = hashlib.sha256(
        json.dumps([config["endpoint"], config["bucket"], sorted(keys)]).encode()
    ).hexdigest()
    return config, keys, digest


def metrics(value):
    if (
        isinstance(value, dict)
        and finite_time(value.get("started_at"))
        and finite_time(value.get("measured_at"))
        and value["started_at"] <= value["measured_at"]
        and integer(value.get("objects"))
        and integer(value.get("bytes"))
    ):
        return {k: value[k] for k in ("objects", "bytes", "started_at", "measured_at")}
    return None


def sanitize(source, keys, digest):
    """Rebuild the private cache too; do not preserve arbitrary metadata."""
    result = dict(schema_version=1, scope=digest, repositories={})
    if not isinstance(source, dict) or source.get("scope") != digest:
        return result
    old = source.get("repositories", {})
    if not isinstance(old, dict):
        return result
    for key in keys:
        value = old.get(key)
        if not isinstance(value, dict):
            continue
        row = {}
        complete = metrics(value.get("complete"))
        if complete:
            row["complete"] = complete
        if finite_time(value.get("attempted_at")):
            row["attempted_at"] = value["attempted_at"]
        if value.get("status") in ("ok", "scanning", "unavailable"):
            row["status"] = value["status"]
        progress = value.get("progress")
        if (
            isinstance(progress, dict)
            and finite_time(progress.get("started_at"))
            and integer(progress.get("objects"))
            and integer(progress.get("bytes"))
            and isinstance(progress.get("token"), str)
            and 0 < len(progress["token"]) <= 16384
        ):
            row["progress"] = {
                k: progress[k] for k in ("started_at", "objects", "bytes", "token")
            }
        result["repositories"][key] = row
    return result


def client(config):
    import boto3
    from botocore.config import Config

    return boto3.client(
        "s3",
        endpoint_url=config["endpoint"],
        region_name="us-east-1",
        aws_access_key_id=Path(os.environ["S3_ACCESS_KEY_FILE"]).read_text().strip(),
        aws_secret_access_key=Path(os.environ["S3_SECRET_KEY_FILE"])
        .read_text()
        .strip(),
        verify=os.environ.get("S3_CA_FILE", True),
        config=Config(
            signature_version="s3v4",
            s3={"addressing_style": "path"},
            connect_timeout=2,
            read_timeout=5,
            retries={"total_max_attempts": 1},
        ),
    )


def scan(config, keys, digest, previous, s3, *, clock=time.time, sleep=time.sleep):
    cache = sanitize(previous, keys, digest)
    rows = cache["repositories"]
    deadline = time.monotonic() + MAX_SECONDS
    # Oldest-attempt-first makes progress fair across large and small repositories.
    pending = sorted(keys, key=lambda k: rows.get(k, {}).get("attempted_at", 0))
    pages = 0
    touched = set()
    next_request = time.monotonic()
    while pending and pages < MAX_PAGES and time.monotonic() < deadline:
        key = pending.pop(0)
        row = rows.setdefault(key, {})
        if clock() - row.get("attempted_at", 0) < INTERVAL:
            continue
        progress = row.get("progress", {})
        if clock() - progress.get("started_at", clock()) > MAX_SCAN_AGE:
            progress = {}
        if not progress:
            progress = dict(started_at=clock(), objects=0, bytes=0)
        prefix = "repositories/" + key + "/"
        try:
            sleep(max(0, next_request - time.monotonic()))
            next_request = time.monotonic() + 1
            arguments = dict(Bucket=config["bucket"], Prefix=prefix, MaxKeys=MAX_KEYS)
            if "token" in progress:
                arguments["ContinuationToken"] = progress["token"]
            pages += 1
            touched.add(key)
            response = s3.list_objects_v2(**arguments)
            contents = response.get("Contents", [])
            if not isinstance(contents, list) or len(contents) > MAX_KEYS:
                raise ValueError("invalid listing")
            count, size = 0, 0
            for obj in contents:
                if (
                    not isinstance(obj.get("Key"), str)
                    or not obj["Key"].startswith(prefix)
                    or not integer(obj.get("Size"))
                ):
                    raise ValueError("invalid object metadata")
                count += 1
                size += obj["Size"]
            count += progress["objects"]
            size += progress["bytes"]
            if not integer(count) or not integer(size):
                raise ValueError("object totals exceed supported range")
            truncated = response.get("IsTruncated")
            if type(truncated) is not bool:
                raise ValueError("missing truncation flag")
            if truncated:
                token = response.get("NextContinuationToken")
                if (
                    not isinstance(token, str)
                    or not 0 < len(token) <= 16384
                    or token == progress.get("token")
                ):
                    raise ValueError("invalid continuation token")
                row.update(
                    status="scanning",
                    progress=dict(
                        started_at=progress["started_at"],
                        objects=count,
                        bytes=size,
                        token=token,
                    ),
                )
                pending.append(key)
            else:
                row["complete"] = dict(
                    started_at=progress["started_at"],
                    measured_at=clock(),
                    objects=count,
                    bytes=size,
                )
                row.pop("progress", None)
                row.update(status="ok", attempted_at=clock())
        except Exception:
            # No exception text: SDK errors can contain hostnames and object keys.
            row.pop("progress", None)
            row.update(status="unavailable", attempted_at=clock())
    for key in touched:
        row = rows[key]
        if row.get("status") == "scanning":
            row["attempted_at"] = clock()
    cache["request_pages"] = pages
    return cache


def summary(root, data_root, now=None):
    """Read-only allowlist consumed by the ordinary host collector."""
    now = time.time() if now is None else now
    try:
        _, keys, digest = scope(root / "private/repositories.yaml")
        cache = sanitize(read_json(data_root / "info/objects-cache.json"), keys, digest)
    except (OSError, ValueError, KeyError, TypeError, AttributeError, yaml.YAMLError):
        return dict(status="unavailable", interval_seconds=INTERVAL, repositories=[])
    rows = []
    for key in keys:
        record = cache["repositories"].get(key, {})
        row = dict(key=key, status=record.get("status", "never"))
        complete = record.get("complete")
        if complete:
            row.update(complete)
            row["age_seconds"] = max(0, round(now - complete["measured_at"]))
            row["stale"] = row["age_seconds"] > INTERVAL * 2
        else:
            row["stale"] = True
        rows.append(row)
    return dict(status="ok", interval_seconds=INTERVAL, repositories=rows)


def main():
    parser = argparse.ArgumentParser(
        description="Cached repository S3 object statistics"
    )
    parser.add_argument("--root", type=Path, default=Path("/app/open-assets"))
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.worker:
        from .__main__ import CONFIG

        config, keys, digest = scope(Path(CONFIG))
        source = sys.stdin.read(4194305)
        if len(source) > 4194304:
            raise ValueError("oversized object cache")
        print(
            json.dumps(scan(config, keys, digest, json.loads(source), client(config)))
        )
        return
    if os.geteuid() != 0:
        parser.error("run the scheduled host collector as root")
    from .telemetry import environment

    root = args.root.resolve()
    directory = Path(environment(root)["DATA_ROOT"]) / "info"
    directory.mkdir(parents=True, exist_ok=True)
    os.umask(0o077)
    with (directory / ".objects.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        _, keys, digest = scope(root / "private/repositories.yaml")
        path = directory / "objects-cache.json"
        try:
            previous = sanitize(read_json(path), keys, digest)
        except (OSError, ValueError, TypeError):
            previous = {}
        try:
            result = subprocess.run(
                [
                    "docker",
                    "compose",
                    "--project-directory",
                    str(root),
                    "exec",
                    "-T",
                    "publisher",
                    "python",
                    "-m",
                    "open_assets.object_info",
                    "--worker",
                ],
                input=json.dumps(previous),
                capture_output=True,
                text=True,
                timeout=130,
            )
            if result.returncode or len(result.stdout) > 4194304:
                raise ValueError("worker failed")
            updated = json.loads(result.stdout)
            if updated.get("scope") != digest:
                raise ValueError("scope changed")
        except (subprocess.SubprocessError, OSError, ValueError, AttributeError):
            failed = sanitize(previous, keys, digest)
            for key in keys:
                failed["repositories"].setdefault(key, {}).update(status="unavailable")
            write_cache(path, failed)
            raise SystemExit(
                "Object statistics worker failed; previous totals retained"
            ) from None
        write_cache(path, sanitize(updated, keys, digest))
        print("Filtered object statistics cache updated")
        if any(
            r.get("status") == "unavailable" for r in updated["repositories"].values()
        ):
            raise SystemExit(1)


if __name__ == "__main__":
    main()
