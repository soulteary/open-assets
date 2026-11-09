"""Reuse proven immutable bytes; missing evidence falls back to full reads."""

import hashlib
import json
from pathlib import Path
import re
import shutil
import stat
import time

VERSION = 3
_audit_next = 0


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


def context(config, repository, audit=False):
    """Admit only the current release under exactly the same validation config."""
    result = dict(oids={}, reused={}, sources={}, reused_files=0, verified={})
    from .__main__ import image_target

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
            or seal.get("manifest")
            != digest(
                dict(
                    files=manifest["files"],
                    object_sources=manifest.get("object_sources", {}),
                )
            )
            or (base / "current").readlink().as_posix()
            != "releases/" + image_target(manifest)
        ):
            return result
        full = base / "releases" / sha
        for parent in (base, base / "releases", full):
            if not stat.S_ISDIR(parent.lstat().st_mode):
                return result
        records = seal["records"]
        tree = {}
        entries_seen = 0
        from .resources import policy

        entry_limit = policy(config)["scan_entries"]
        stack = [(full, "")]
        while stack:
            directory, prefix = stack.pop()
            import os

            with os.scandir(directory) as entries:
                for entry in entries:
                    entries_seen += 1
                    if entries_seen > entry_limit:
                        raise ValueError("incremental scan budget exceeded")
                    info = entry.stat(follow_symlinks=False)
                    name = prefix + entry.name
                    if stat.S_ISDIR(info.st_mode):
                        stack.append((Path(entry.path), name + "/"))
                    elif stat.S_ISREG(info.st_mode):
                        tree[name] = info
                    else:
                        return result
        if set(tree) != {item["path"] for item in manifest["files"]}:
            return result
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
            info = tree[path]
            if [
                info.st_dev,
                info.st_ino,
                info.st_size,
                info.st_mtime_ns,
                info.st_ctime_ns,
            ] != record.get("fingerprint") or info.st_size != item["size"]:
                continue
            from . import object_refs, immutable

            object_source = object_refs.source(manifest, item)
            byte_proof = record.get("immutable")
            if object_refs.enabled(config) and not immutable.matches_stat(
                info, item, byte_proof, allow_expired=audit
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
                    key=object_source["key"],
                    object_content_type=object_source["content_type"],
                    verified=byte_proof,
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
    if context.get("reference_mode"):
        from . import immutable

        context["verified"][relative.as_posix()] = immutable.link(
            source, destination, record["verified"]
        )
    else:
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
                immutable=context["verified"].get(name),
            )
    atomic(
        Path(config["state_root"]) / "incremental" / (key + ".json"),
        dict(
            version=VERSION,
            sha=sha,
            config=config["revision"],
            manifest=digest(
                dict(files=files, object_sources=context.get("object_sources", {}))
            ),
            records=records,
        ),
    )


def audit_idle(config):
    """Renew full-byte proofs while idle, in bounded slices, yielding to pushes.

    Reuse never extends a proof's byte-verification time. A restart or failed
    audit can cause a safe full export rather than indefinitely trusted bytes.
    """
    global _audit_next
    from . import immutable, object_refs
    from .__main__ import atomic, read_job

    if not object_refs.enabled(config) or time.monotonic() < _audit_next:
        return
    _audit_next = time.monotonic() + 15
    deadline = time.monotonic() + 2
    state = Path(config["state_root"])

    def waiting():
        return any(
            read_job(p)["status"] in ("pending", "running", "images_prepared")
            for p in (state / "jobs").glob("*.json")
        )

    for repository in config["repositories"]:
        if time.monotonic() >= deadline or waiting():
            return
        try:
            value = context(config, repository, audit=True)
            path = state / "incremental" / (repository["key"] + ".json")
            if not value["sources"]:
                continue
            seal = object_refs.read(path)
            changed = False
            for source in value["sources"].values():
                proof = source["verified"]
                if time.time() - proof["verified_at"] < immutable.MAX_PROOF_AGE / 2:
                    continue
                if time.monotonic() >= deadline or waiting():
                    break
                digest = hashlib.sha256()
                complete = True
                with source["source"].open("rb") as stream:
                    for block in iter(lambda: stream.read(1048576), b""):
                        if time.monotonic() >= deadline:
                            complete = False
                            break
                        digest.update(block)
                if not complete:
                    break
                valid = digest.hexdigest() == proof["sha256"] and immutable.matches(
                    source["source"], source["item"], proof, allow_expired=True
                )
                for name, record in list(seal["records"].items()):
                    current = record.get("immutable")
                    if current and current["fingerprint"] == proof["fingerprint"]:
                        if valid:
                            current["verified_at"] = time.time()
                        else:
                            del seal["records"][name]
                        changed = True
            if changed:
                atomic(path, seal)
        except (OSError, ValueError, KeyError, TypeError):
            # Publication independently refuses stale or invalid cache evidence.
            continue
