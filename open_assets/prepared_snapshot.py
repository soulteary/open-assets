"""One unserved, durable hardlink view prepared while the publisher is idle."""

import copy
import os
from pathlib import Path
import re
import shutil
import stat
import time
import uuid

from . import immutable, object_refs


def state_path(config, key):
    return Path(config["state_root"]) / "prepared" / f"{key}.json"


def public(record):
    return {k: v for k, v in record.items() if not k.startswith("_")}


def candidate(config, repository):
    try:
        path = state_path(config, repository["key"])
        record = object_refs.read(path)
        if record.get("schema_version") != 1 or record.get("status") != "ready":
            return None
        name = record.get("directory", "")
        if not isinstance(name, str) or not re.fullmatch(
            r"\.staging-prefetch-[a-f0-9]{32}", name
        ):
            return None
        root = Path(config["images_root"]) / "repos" / repository["key"] / name
        if not stat.S_ISDIR(root.lstat().st_mode):
            return None
        return dict(record, _root=root, _state=path)
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        return None


def dirty(reuse, destination, directory):
    if "snapshot_dirty" not in reuse:
        return
    while True:
        relative = directory.relative_to(destination)
        reuse["snapshot_dirty"].add(relative)
        if directory == destination:
            break
        directory = directory.parent


def claim(config, destination, blobs, reuse, sha):
    record = (reuse or {}).get("prepared")
    if not record or record["sha"] == sha:
        return {}
    from . import __main__ as core

    updated = public(record)
    updated["status"] = "claimed"
    core.atomic(record["_state"], updated)
    os.replace(record["_root"], destination)
    reuse["snapshot_dirty"] = {Path(".")}
    wanted = {relative.as_posix(): oid.decode() for oid, relative, _, _ in blobs}
    # Unlink only the private view. Refresh ctime only after proving our own unlink.
    for name, oid in record["oids"].items():
        if wanted.get(name) == oid:
            continue
        source = reuse["sources"][oid]
        immutable.unlink_view(destination / name, source["source"], source["verified"])
        parent = (destination / name).parent
        dirty(reuse, destination, parent)
        while parent != destination:
            try:
                parent.rmdir()
            except OSError:
                break
            reuse["snapshot_dirty"].discard(parent.relative_to(destination))
            parent = parent.parent
            dirty(reuse, destination, parent)
    items = {item["path"]: item for item in record["files"]}
    retained = {}
    for index, (oid, relative, _, size) in enumerate(blobs):
        name = relative.as_posix()
        if record["oids"].get(name) != oid.decode():
            continue
        source = reuse["sources"][oid.decode()]
        if items[name]["size"] != size:
            raise ValueError("prepared blob size changed")
        retained[index] = dict(items[name])
        reuse["reused"][name] = source
        reuse["verified"][name] = source["verified"]
        reuse["reused_files"] += 1
    for index, (_, relative, _, _) in enumerate(blobs):
        if index not in retained:
            dirty(reuse, destination, (destination / relative).parent)
    reuse["snapshot_retained"] = len(retained)
    return retained


def discard(config, repository, record):
    """Remove only our checkpointed private view, proving each shared inode delta."""
    from . import __main__ as core, incremental

    name = record.get("directory", "")
    if not re.fullmatch(r"\.staging-prefetch-[a-f0-9]{32}", name):
        raise ValueError("unsafe prepared directory")
    state = Path(config["state_root"])
    manifest = object_refs.read(state / "published" / repository["key"])
    seal_path = state / "incremental" / (repository["key"] + ".json")
    seal = object_refs.read(seal_path)
    if (
        seal["sha"] != manifest["sha"]
        or seal["config"] != config["revision"]
        or seal["manifest"]
        != incremental.digest(
            dict(
                files=manifest["files"],
                object_sources=manifest.get("object_sources", {}),
            )
        )
    ):
        raise ValueError("prepared cleanup baseline changed")
    base = Path(config["images_root"]) / "repos" / repository["key"]
    full = base / "releases" / manifest["sha"]
    proofs = {n: v["immutable"] for n, v in seal["records"].items()}
    core.verify_tree(full, manifest["files"], verified=proofs)
    tracked = {}
    for item in manifest["files"]:
        proof = proofs[item["path"]]
        identity = tuple(proof["fingerprint"][:2])
        tracked.setdefault(
            identity, dict(source=full / item["path"], proof=proof, count=0)
        )
    root = base / name
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("unsafe prepared cleanup root")
    entries = 0
    for directory, dirs, files in os.walk(root, followlinks=False):
        entries += 1 + len(files) + len(dirs)
        if entries > 200000:
            raise ValueError("prepared cleanup entry budget exceeded")
        if not stat.S_ISDIR(os.lstat(directory).st_mode):
            raise ValueError("unsafe prepared cleanup directory")
        for filename in files:
            info = os.lstat(Path(directory) / filename)
            shared = tracked.get((info.st_dev, info.st_ino))
            if shared is not None:
                if not immutable.matches_stat(info, shared["proof"], shared["proof"]):
                    raise ValueError("prepared cleanup identity changed")
                shared["count"] += 1
    for value in tracked.values():
        info = os.lstat(value["source"])
        if not immutable.matches_stat(info, value["proof"], value["proof"]):
            raise ValueError("prepared cleanup source changed")
        value["before"] = info.st_nlink
    # rmtree's descriptor-based implementation does not follow directory links.
    if not shutil.rmtree.avoids_symlink_attacks:
        raise ValueError("safe prepared cleanup unavailable")
    shutil.rmtree(root)
    changed = {}
    for identity, value in tracked.items():
        if not value["count"]:
            continue
        info = os.lstat(value["source"])
        current, old = immutable.identity(info), value["proof"]["fingerprint"]
        if (
            current[:4] != old[:4]
            or current[5:] != old[5:]
            or info.st_nlink != value["before"] - value["count"]
        ):
            raise ValueError("prepared cleanup changed source")
        changed[identity] = current
    for value in seal["records"].values():
        current = changed.get(tuple(value["immutable"]["fingerprint"][:2]))
        if current is not None:
            value["fingerprint"] = current[:5]
            value["immutable"]["fingerprint"] = current[:]
    core.verify_tree(
        full,
        manifest["files"],
        verified={n: v["immutable"] for n, v in seal["records"].items()},
    )
    core.atomic(seal_path, seal)
    state_path(config, repository["key"]).unlink()


def prepare_idle(config):
    """Caller holds the publisher lock. Failures disable this optional view."""
    if not object_refs.enabled(config):
        return
    from . import __main__ as core, incremental

    state = Path(config["state_root"])

    def waiting():
        return any(
            core.read_job(p)["status"] in ("pending", "running", "images_prepared")
            for p in (state / "jobs").glob("*.json")
        )

    for repository in config["repositories"]:
        if waiting():
            return
        if not repository.get("enabled", True):
            continue
        key = repository["key"]
        path = state_path(config, key)
        try:
            if path.exists() or path.is_symlink():
                old = object_refs.read(path)
                name = old.get("directory", "")
                if (
                    re.fullmatch(r"\.staging-prefetch-[a-f0-9]{32}", name)
                    and not (
                        Path(config["images_root"]) / "repos" / key / name
                    ).exists()
                    and not (
                        Path(config["images_root"]) / "repos" / key / name
                    ).is_symlink()
                ):
                    path.unlink()
                elif (
                    old.get("sha") != object_refs.read(state / "published" / key)["sha"]
                ):
                    discard(config, repository, old)
                else:
                    continue
            manifest = object_refs.read(state / "published" / key)
            if manifest.get("image_layout") != "snapshot-v1":
                continue
            seal = object_refs.read(state / "incremental" / f"{key}.json")
            full = (
                Path(config["images_root"])
                / "repos"
                / key
                / "releases"
                / manifest["sha"]
            )
            reuse = incremental.context(config, repository, force_current=True)
            reuse["reference_mode"] = True
            reuse["hardlink_only"] = True
            if not reuse["sources"] or any(
                seal["records"][item["path"]]["oid"] not in reuse["sources"]
                for item in manifest["files"]
            ):
                continue
            core.verify_tree(
                full,
                manifest["files"],
                verified={
                    name: value["immutable"] for name, value in seal["records"].items()
                },
            )
            if waiting():
                return
            name = ".staging-prefetch-" + uuid.uuid4().hex
            root = full.parent.parent / name
            record = dict(
                schema_version=1,
                status="building",
                directory=name,
                sha=manifest["sha"],
                config=config["revision"],
                manifest=seal["manifest"],
            )
            core.atomic(path, record)
            root.mkdir(mode=0o700)
            deadline = time.monotonic() + 5

            def check():
                if time.monotonic() > deadline:
                    raise ValueError("prepared snapshot time budget exceeded")

            files = core.export(
                config,
                repository,
                state / "git" / key,
                manifest["sha"],
                root,
                check=check,
                reuse=reuse,
            )
            if files != manifest["files"] or reuse["reused_files"] != len(files):
                raise ValueError("prepared snapshot provenance incomplete")
            core.verify_tree(root, files, verified=reuse["verified"])
            core.sync_tree(root, data_synced=True)
            from .retention import sync_directory

            sync_directory(root.parent)
            # Every ctime change below was caused by a checked link. Never extend
            # the original full-byte verification time or change manifest bytes.
            changed = {
                tuple(v["verified"]["fingerprint"][:2]): v["verified"]
                for v in reuse["sources"].values()
            }
            refreshed = copy.deepcopy(seal)
            for value in refreshed["records"].values():
                old_proof = value["immutable"]
                proof = changed.get(tuple(old_proof["fingerprint"][:2]))
                if proof is not None:
                    if (
                        proof["sha256"] != old_proof["sha256"]
                        or proof["size"] != old_proof["size"]
                    ):
                        raise ValueError("prepared inode identity collision")
                    value["fingerprint"] = proof["fingerprint"][:5]
                    old_proof["fingerprint"] = proof["fingerprint"][:]
            core.verify_tree(
                full,
                files,
                verified={n: v["immutable"] for n, v in refreshed["records"].items()},
            )
            core.atomic(state / "incremental" / f"{key}.json", refreshed)
            record.update(
                status="ready",
                files=files,
                oids=reuse["oids"],
                proofs=reuse["verified"],
            )
            core.atomic(path, record)
        except (OSError, ValueError, KeyError, TypeError, AttributeError):
            # Keep at most the checkpointed view; do not unlink shared inodes
            # behind an existing receipt. Normal publication remains available.
            continue
