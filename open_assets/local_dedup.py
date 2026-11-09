"""One-off verified hardlink deduplication. Logical snapshots remain complete."""

import hashlib
import os
from pathlib import Path
import stat
import uuid

from . import immutable, object_refs, retention


def run(config, apply=False):
    from . import __main__ as core

    report = dict(apply=apply, files=0, replaced=0, reclaimable_bytes=0)
    state = Path(config["state_root"])
    with core.lock(config):
        jobs = state / "jobs"
        if any(
            core.read_job(p)["status"] in ("pending", "running", "images_prepared")
            for p in jobs.glob("*.json")
        ):
            raise ValueError("wait for queued publications before local deduplication")
        groups = {}
        paths = []
        hashed = {}
        # Complete preflight before replacing a single directory entry.
        for repository in config["repositories"]:
            key = repository["key"]
            base = Path(config["images_root"]) / "repos" / key
            folder = state / "manifests" / key
            if not folder.exists():
                continue
            retention.real_directory(folder)
            for filename in sorted(folder.glob("*.json")):
                manifest = object_refs.read(filename)
                object_refs.validate(manifest)
                if manifest["key"] != key or filename.stem != manifest["sha"]:
                    raise ValueError("dedup manifest identity mismatch")
                sha = manifest["sha"]
                for root, files in (
                    (base / "releases" / sha, manifest["files"]),
                    (
                        base / "releases" / ("images-" + sha),
                        core.image_files(manifest["files"]),
                    ),
                ):
                    if not root.exists() and not root.is_symlink():
                        continue
                    for parent in (base.parent, base, base / "releases", root):
                        retention.real_directory(parent)
                    actual = {}
                    for directory, dirs, names in os.walk(root, followlinks=False):
                        for name in dirs:
                            retention.real_directory(Path(directory) / name)
                        for name in names:
                            p = Path(directory) / name
                            if not stat.S_ISREG(p.lstat().st_mode):
                                raise ValueError("unsafe dedup entry")
                            actual[p.relative_to(root).as_posix()] = p
                    if set(actual) != {f["path"] for f in files}:
                        raise ValueError("dedup tree differs from manifest")
                    for item in files:
                        p = actual[item["path"]]
                        fingerprint = immutable.fingerprint(p)
                        identity = tuple(fingerprint)
                        if identity not in hashed:
                            digest = hashlib.sha256()
                            with p.open("rb") as stream:
                                for block in iter(lambda: stream.read(1048576), b""):
                                    digest.update(block)
                            if immutable.fingerprint(p) != fingerprint:
                                raise ValueError("dedup source changed while hashing")
                            hashed[identity] = digest.hexdigest()
                        if (
                            fingerprint[2] != item["size"]
                            or hashed[identity] != item["sha256"]
                        ):
                            raise ValueError("dedup content differs from manifest")
                        group = (key, item["sha256"], item["size"])
                        groups.setdefault(group, []).append((p, fingerprint, item))
                        paths.append(p)
                        report["files"] += 1
        # Logical entries may already share inodes. Count each redundant physical
        # inode once; application data outside these manifests is never touched.
        for entries in groups.values():
            source, initial, item = entries[0]
            seen = {tuple(initial[:2])}
            for _, proof, _ in entries[1:]:
                inode = tuple(proof[:2])
                if inode not in seen:
                    report["reclaimable_bytes"] += item["size"]
                    seen.add(inode)
            if not apply:
                continue
            for p, before, _ in entries:
                if immutable.fingerprint(p) != before:
                    raise ValueError("dedup preflight identity changed")
            proof = immutable.record(source, item)
            for p, before, _ in entries[1:]:
                if immutable.fingerprint(p)[:2] == proof["fingerprint"][:2]:
                    continue
                # Earlier replacements can alter the old inode's ctime/nlink,
                # but never its size, mtime or mode. Track those controlled changes.
                current = immutable.fingerprint(p)
                if current[:4] != before[:4] or current[5:] != before[5:]:
                    raise ValueError("dedup target changed")
                temporary = p.with_name(".dedup-" + uuid.uuid4().hex)
                try:
                    immutable.link(source, temporary, proof)
                    if immutable.fingerprint(p) != current:
                        raise ValueError("dedup target changed before replace")
                    os.replace(temporary, p)
                    after = immutable.fingerprint(source)
                    if (
                        after[:4] != proof["fingerprint"][:4]
                        or after[5:] != proof["fingerprint"][5:]
                    ):
                        raise ValueError("dedup source changed during replacement")
                    proof["fingerprint"][:] = after
                    retention.sync_directory(p.parent)
                    report["replaced"] += 1
                finally:
                    temporary.unlink(missing_ok=True)
        if apply:
            # Hardlink and mode changes invalidate old inode proofs. They are
            # optimization receipts; keep a journal and require fresh validation.
            for repository in config["repositories"]:
                p = state / "incremental" / (repository["key"] + ".json")
                if p.exists():
                    core.atomic(
                        p, dict(version=0, reason="verified local deduplication")
                    )
            core.atomic(state / "local-dedup.json", report)
    return report


def legacy_views(config, apply=False):
    """Materialize old directory names before an older binary is restored."""
    import shutil
    import tempfile
    from . import __main__ as core
    from .publication import link_or_copy

    report = dict(apply=apply, versions=0)
    state = Path(config["state_root"])
    with core.lock(config):
        if any(
            core.read_job(p)["status"] in ("pending", "running", "images_prepared")
            for p in (state / "jobs").glob("*.json")
        ):
            raise ValueError("wait for queued publications before materialization")
        for repository in config["repositories"]:
            key = repository["key"]
            base = Path(config["images_root"]) / "repos" / key
            for filename in (state / "manifests" / key).glob("*.json"):
                manifest = object_refs.read(filename)
                object_refs.validate(manifest)
                if manifest["key"] != key or filename.stem != manifest["sha"]:
                    raise ValueError("materialization manifest mismatch")
                if manifest.get("image_layout") != "snapshot-v1":
                    continue
                sha = manifest["sha"]
                full = base / "releases" / sha
                if not full.exists() and not full.is_symlink():
                    continue
                core.verify_tree(full, manifest["files"])
                images = core.image_files(manifest["files"])
                target = base / "releases" / ("images-" + sha)
                if target.exists() or target.is_symlink():
                    core.verify_tree(target, images)
                    continue
                report["versions"] += 1
                if not apply:
                    continue
                temporary = Path(tempfile.mkdtemp(prefix=".legacy-view-", dir=base))
                try:
                    shutil.copytree(
                        full / "images",
                        temporary,
                        dirs_exist_ok=True,
                        copy_function=link_or_copy,
                    )
                    core.verify_tree(temporary, images)
                    core.sync_tree(temporary, data_synced=True)
                    os.rename(temporary, target)
                    retention.sync_directory(target.parent)
                finally:
                    shutil.rmtree(temporary, ignore_errors=True)
            if apply:
                current = base / "current"
                if current.is_symlink() and current.readlink().as_posix().endswith(
                    "/images"
                ):
                    sha = retention.current_sha(base)
                    core.switch(base, "images-" + sha)
                cache = state / "incremental" / (key + ".json")
                if cache.exists():
                    core.atomic(
                        cache, dict(version=0, reason="legacy view materialization")
                    )
    return report
