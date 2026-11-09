"""Reuse verified bytes with sealed local identity and conditional S3 copies.

Release object paths remain complete and independent; retention needs no graph.
The seal is an optimization only. Missing/stale evidence falls back to full reads.
"""

import hashlib
import json
from pathlib import Path
import re
import shutil
import stat

VERSION = 2


def fingerprint(path):
    value = path.lstat()
    if not stat.S_ISREG(value.st_mode):
        raise ValueError("unsafe incremental source")
    return [
        value.st_dev,
        value.st_ino,
        value.st_size,
        value.st_mtime_ns,
        value.st_ctime_ns,
    ]


def digest(files):
    return hashlib.sha256(json.dumps(files, sort_keys=True).encode()).hexdigest()


def valid_etag(value):
    return isinstance(value, str) and re.fullmatch(r'"[0-9a-f]{32}"', value) is not None


def context(config, repository):
    """Admit only the current release under exactly the same validation config."""
    result = dict(oids={}, reused={}, sources={}, reused_files=0)
    key = repository["key"]
    state = Path(config["state_root"])
    base = Path(config["images_root"]) / "repos" / key
    try:
        published_path = state / "published" / key
        seal_path = state / "incremental" / (key + ".json")
        for path in (published_path, seal_path):
            if (
                not stat.S_ISREG(path.lstat().st_mode)
                or path.stat().st_size > 8 * 1024**2
            ):
                return result
        manifest = json.loads(published_path.read_text())
        seal = json.loads(seal_path.read_text())
        sha = manifest["sha"]
        if (
            not re.fullmatch(r"[0-9a-f]{40}", sha)
            or manifest["key"] != key
            or seal.get("version") != VERSION
            or seal.get("sha") != sha
            or seal.get("config") != config["revision"]
            or seal.get("manifest") != digest(manifest["files"])
            or (base / "current").readlink().as_posix() != "releases/images-" + sha
        ):
            return result
        full = base / "releases" / sha
        for parent in (base, base / "releases", full):
            if not stat.S_ISDIR(parent.lstat().st_mode):
                return result
        records = seal["records"]
        for item in manifest["files"]:
            path = item["path"]
            relative = Path(path)
            if (
                relative.is_absolute()
                or relative.as_posix() != path
                or any(p.startswith(".") for p in relative.parts)
            ):
                return result
            record = records.get(path, {})
            oid = record.get("oid", "")
            if not re.fullmatch(r"[0-9a-f]{40}", oid) or not valid_etag(
                record.get("etag")
            ):
                continue
            modified = record.get("modified")
            if modified is not None and (type(modified) is not int or modified < 0):
                continue
            source = full / path
            if any(
                not stat.S_ISDIR(p.lstat().st_mode)
                for p in source.parents
                if p != full and full in p.parents
            ):
                continue
            if (
                fingerprint(source) != record.get("fingerprint")
                or source.stat().st_size != item["size"]
            ):
                continue
            result["sources"].setdefault(
                oid,
                dict(
                    item=item,
                    source=source,
                    fingerprint=record["fingerprint"],
                    etag=record["etag"],
                    modified=record.get("modified"),
                    key=f"repositories/{key}/releases/{sha}/{path}",
                ),
            )
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        # An incomplete or damaged seal must never turn into trusted evidence.
        result["sources"] = {}
    return result


def copy_cached(context, oid, size, relative, destination):
    record = context["sources"].get(oid.decode())
    if not record or record["item"]["size"] != size:
        return None
    source = record["source"]
    if fingerprint(source) != record["fingerprint"]:
        return None
    shutil.copyfile(source, destination)
    if fingerprint(source) != record["fingerprint"]:
        raise ValueError("incremental source changed during copy")
    item = dict(record["item"], path=relative.as_posix())
    context["reused"][item["path"]] = record
    context["reused_files"] += 1
    return item


def save(config, key, sha, files, context, etags):
    from .__main__ import atomic

    full = Path(config["images_root"]) / "repos" / key / "releases" / sha
    records = {}
    for item in files:
        name = item["path"]
        value = etags.get(name) or {}
        if valid_etag(value.get("etag")):
            modified = value.get("modified")
            if modified is not None and (type(modified) is not int or modified < 0):
                continue
            records[name] = dict(
                oid=context["oids"][name],
                etag=value["etag"],
                modified=modified,
                fingerprint=fingerprint(full / name),
            )
    atomic(
        Path(config["state_root"]) / "incremental" / (key + ".json"),
        dict(
            version=VERSION,
            sha=sha,
            config=config["revision"],
            manifest=digest(files),
            records=records,
        ),
    )
