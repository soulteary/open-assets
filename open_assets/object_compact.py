"""Verified one-off object compaction; keep every logical release and URL."""

import base64
import concurrent.futures
import hashlib
from pathlib import Path
import uuid
import urllib.request

from . import object_refs, retention


def delete_checksum(params, **kwargs):
    body = params["body"]
    if not isinstance(body, bytes):
        raise ValueError("unexpected bulk deletion body")
    params["headers"]["Content-MD5"] = base64.b64encode(
        hashlib.md5(body, usedforsecurity=False).digest()
    ).decode("ascii")


def reader_ready():
    with urllib.request.urlopen(
        "http://object-reader:8083/healthz", timeout=3
    ) as response:
        if (
            response.status != 200
            or response.headers.get("X-Open-Assets-Reader") != "guard-v1"
        ):
            raise ValueError("upgrade the object reader before physical compaction")


def run(config, client, apply=False):
    from . import __main__ as core
    from .resources import verify_body

    if getattr(client, "meta", None) is not None:
        client.meta.events.register(
            "before-call.s3.DeleteObjects",
            delete_checksum,
            unique_id="open-assets-compaction-md5",
        )

    if apply:
        reader_ready()
    state = Path(config["state_root"])
    report = dict(
        apply=apply, objects_before=0, objects_after=0, redundant_bytes=0, deleted=0
    )
    if client.get_bucket_versioning(Bucket=config["bucket"]).get("Status") not in (
        None,
        "",
    ):
        raise ValueError("compaction requires an unversioned bucket")
    with core.lock(config):
        if any(
            core.read_job(p)["status"] in ("pending", "running", "images_prepared")
            for p in (state / "jobs").glob("*.json")
        ):
            raise ValueError("wait for queued publications before object compaction")
        plans = []
        expected = {}
        for repository in config["repositories"]:
            key = repository["key"]
            folder = state / "manifests" / key
            if not folder.exists():
                continue
            retention.real_directory(folder)
            current_path = state / "published" / key
            if not current_path.exists():
                continue
            current = object_refs.read(current_path)
            if current.get("object_layout") != "references-v1":
                report.setdefault("skipped_repositories", []).append(key)
                continue
            paths = list(folder.glob("*.json"))
            paths.sort(key=lambda p: (p.stem != current["sha"], p.name))
            canonical = {}
            for path in paths:
                original = object_refs.read(path)
                refs = object_refs.validate(original)
                if original["key"] != key or original["sha"] != path.stem:
                    raise ValueError("compaction manifest mismatch")
                marker = state / "object-retention" / key / (path.stem + ".json")
                if marker.exists():
                    receipt = object_refs.read(marker)
                    if receipt.get("status") == "complete":
                        continue
                    if receipt.get("status") == "deleting":
                        raise ValueError("object deletion already in progress")
                updated = dict(
                    original, object_layout="references-v1", object_sources={}
                )
                for item in original["files"]:
                    source = refs[item["path"]]
                    identity = (key, item["sha256"], item["size"])
                    owner = canonical.setdefault(identity, source)
                    existing = expected.setdefault(
                        source["key"], dict(item, content_type=source["content_type"])
                    )
                    if (
                        existing["sha256"],
                        existing["size"],
                        existing["content_type"],
                    ) != (item["sha256"], item["size"], source["content_type"]):
                        raise ValueError("inconsistent source ownership")
                    own = f"repositories/{key}/releases/{path.stem}/{item['path']}"
                    if owner["key"] != own:
                        updated["object_sources"][item["path"]] = owner
                plans.append((path, original, updated))
                if len(expected) > 100000 or len(plans) > 10000:
                    raise ValueError("object compaction budget exceeded")
            # Changing current object provenance would invalidate its optimized
            # evidence. Prefer its sources and refuse an ambiguous current state.
            matching = [
                new
                for _, old, new in plans
                if old["key"] == key and old["sha"] == current["sha"]
            ]
            if matching != [current]:
                raise ValueError(
                    "current manifest must already use canonical source objects"
                )
        if len(expected) > 100000 or len(plans) > 10000:
            raise ValueError("object compaction budget exceeded")
        # Bound in-flight full-byte verification. Nothing is rewritten or deleted
        # unless all original source objects have passed SHA-256 and MIME checks.
        iterator = iter(expected.items())

        def verify(pair):
            name, item = pair
            response = client.get_object(Bucket=config["bucket"], Key=name)
            if response.get("ContentType") != item["content_type"]:
                response["Body"].close()
                raise ValueError("source MIME changed")
            verify_body(response["Body"], item)

        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            pending = set()
            while True:
                while len(pending) < 4:
                    pair = next(iterator, None)
                    if pair is None:
                        break
                    pending.add(pool.submit(verify, pair))
                if not pending:
                    break
                done, pending = concurrent.futures.wait(
                    pending, return_when=concurrent.futures.FIRST_COMPLETED
                )
                for task in done:
                    task.result()
        used = {
            v["key"] for _, _, new in plans for v in object_refs.validate(new).values()
        }
        redundant = sorted(set(expected) - used)
        report.update(
            objects_before=len(expected),
            objects_after=len(used),
            redundant_bytes=sum(expected[k]["size"] for k in redundant),
        )
        if not apply:
            return report
        if any(object_refs.read(path) != old for path, old, _ in plans):
            raise ValueError("manifest changed during compaction")
        journal = state / "object-compaction" / uuid.uuid4().hex
        journal.parent.mkdir(exist_ok=True, mode=0o700)
        retention.real_directory(journal.parent)
        journal.mkdir(mode=0o700)
        # Metadata backups are for recovery/audit. Never restore old manifests
        # after deletion without first restoring their old physical objects.
        core.atomic(
            journal / "plan.json",
            dict(bucket=config["bucket"], redundant=redundant, report=report),
        )
        # New reader holds the same shared lock from resolution to response end.
        # Take the exclusive lock before changing metadata and deleting any key.
        with object_refs.read_guard(
            state / "object-retention/compaction.lock", exclusive=True, required=True
        ):
            for path, old, new in plans:
                core.atomic(journal / (old["key"] + "-" + old["sha"] + ".json"), old)
                if new != old:
                    core.atomic(path, new)
            core.atomic(
                journal / "committed.json",
                dict(status="all manifests committed", objects=sorted(used)),
            )
        for offset in range(0, len(redundant), 500):
            batch = redundant[offset : offset + 500]
            response = client.delete_objects(
                Bucket=config["bucket"],
                Delete=dict(Objects=[dict(Key=k) for k in batch], Quiet=False),
            )
            if response.get("Errors") or {
                x["Key"] for x in response.get("Deleted", [])
            } != set(batch):
                raise ValueError("partial object compaction deletion; inspect journal")
            report["deleted"] += len(batch)
            core.atomic(journal / "result.json", report)
        core.atomic(journal / "result.json", report)
        return report
