"""Bounded object transfers and local immutable snapshot image views."""

from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import base64
import datetime
import errno
import os
import shutil
import stat

from .resources import verify_body


def policy(config):
    value = config.get("publication", {})
    if not isinstance(value, dict) or set(value) - {
        "object_workers",
        "reference_releases",
    }:
        raise ValueError("invalid publication settings")
    if type(value.get("reference_releases", False)) is not bool:
        raise ValueError("publication.reference_releases must be a boolean")
    workers = value.get("object_workers", 4)
    if type(workers) is not int or not 1 <= workers <= 8:
        raise ValueError("publication.object_workers must be between 1 and 8")
    return workers


def proof(response, etag):
    modified = response.get("LastModified")
    return dict(
        etag=etag,
        modified=int(modified.timestamp())
        if isinstance(modified, datetime.datetime)
        else None,
    )


def transfer(client, bucket, prefix, staging, item, cached=None, existing_keys=None):
    key = prefix + item["path"]
    if cached and existing_keys is not None and key not in existing_keys:
        args = dict(
            Bucket=bucket,
            Key=key,
            CopySource={"Bucket": bucket, "Key": cached["key"]},
            CopySourceIfMatch=cached["etag"],
            MetadataDirective="REPLACE",
            Metadata={"sha256": item["sha256"]},
            ContentType=item["content_type"],
            CacheControl="public, max-age=31536000, immutable",
        )
        if cached.get("modified") is not None:
            args["CopySourceIfUnmodifiedSince"] = datetime.datetime.fromtimestamp(
                cached["modified"], datetime.timezone.utc
            )
        try:
            response = client.copy_object(**args, ChecksumAlgorithm="SHA256")
        except Exception as error:
            code = getattr(error, "response", {}).get("Error", {}).get("Code")
            if code not in ("NotImplemented", "InvalidArgument", "InvalidRequest"):
                raise
            response = client.copy_object(**args)
        etag = response.get("CopyObjectResult", {}).get("ETag")
        if etag != cached["etag"]:
            raise ValueError("incremental copy integrity mismatch")
        checksum = response.get("CopyObjectResult", {}).get("ChecksumSHA256")
        expected = base64.b64encode(bytes.fromhex(item["sha256"])).decode()
        if checksum is not None and checksum != expected:
            raise ValueError("incremental copy SHA-256 mismatch")
        if checksum is None and cached.get("modified") is None:
            # Old/incomplete receipts cannot authenticate an unchanged source.
            readback = client.get_object(Bucket=bucket, Key=key)
            verified = verify_body(readback["Body"], item)
            return proof(
                readback, verified if verified == readback.get("ETag") else None
            ), "copied-readback"
        mode = "copied" if checksum is not None else "copied-verified-source"
        return proof(response["CopyObjectResult"], etag), mode

    try:
        existing = client.get_object(Bucket=bucket, Key=key)
    except client.exceptions.NoSuchKey:
        existing = None
    if existing is not None:
        etag = verify_body(existing["Body"], item)
        return proof(
            existing, etag if etag == existing.get("ETag") else None
        ), "existing"
    else:
        with (staging / item["path"]).open("rb") as stream:
            client.put_object(
                Bucket=bucket,
                Key=key,
                Body=stream,
                ContentLength=item["size"],
                ContentType=item["content_type"],
                CacheControl="public, max-age=31536000, immutable",
                Metadata={"sha256": item["sha256"]},
            )
        response = client.get_object(Bucket=bucket, Key=key)
        etag = verify_body(response["Body"], item)
        return proof(
            response, etag if etag == response.get("ETag") else None
        ), "uploaded"


def upload(
    config, client, prefix, staging, files, reuse=None, stats=None, source_check=None
):
    """At most N active futures; drain running work before exposing failure."""
    workers = policy(config)
    from . import object_refs

    if source_check is not None and (
        not object_refs.enabled(config) or source_check[0] != reuse
    ):
        raise ValueError("source verification plan changed")
    if object_refs.enabled(config) and reuse:
        if source_check is None:
            checked = object_refs.verify_sources(config, client, list(reuse.values()))
        else:
            expected, future = source_check
            if expected != reuse:
                raise ValueError("source verification plan changed")
            checked = future.result()
        if stats is not None:
            stats.update(checked)
        transfers = [item for item in files if item["path"] not in reuse]
        result = upload(config, client, prefix, staging, transfers, stats=stats)
        for name, cached in reuse.items():
            result[name] = dict(etag=cached["etag"], modified=cached["modified"])
        if stats is not None:
            stats["referenced"] = len(reuse)
        return result
    iterator = iter(files)
    pending = {}
    etags = {}
    stats = stats if stats is not None else {}
    existing_keys = None
    if reuse:
        expected = {prefix + item["path"] for item in files}
        existing_keys = set()
        token = None
        seen = set()
        while True:
            args = dict(Bucket=config["bucket"], Prefix=prefix, MaxKeys=1000)
            if token:
                args["ContinuationToken"] = token
            page = client.list_objects_v2(**args)
            for item in page.get("Contents", []):
                name = item["Key"]
                if name not in expected or name in existing_keys:
                    raise ValueError("unexpected incremental destination object")
                existing_keys.add(name)
            if page.get("IsTruncated") is False:
                break
            token = page.get("NextContinuationToken")
            if not isinstance(token, str) or not token or token in seen:
                raise ValueError("invalid incremental destination pagination")
            seen.add(token)
            if len(seen) > len(files):
                raise ValueError("incremental listing limit")
    with ThreadPoolExecutor(max_workers=workers) as pool:

        def submit():
            item = next(iterator, None)
            if item is not None:
                future = pool.submit(
                    transfer,
                    client,
                    config["bucket"],
                    prefix,
                    staging,
                    item,
                    (reuse or {}).get(item["path"]),
                    existing_keys,
                )
                pending[future] = item["path"]

        try:
            for _ in range(workers):
                submit()
            while pending:
                done, _ = wait(pending, return_when=FIRST_COMPLETED)
                # Observe every completed future before submitting more work.
                for future in done:
                    etag, mode = future.result()
                    etags[pending.pop(future)] = etag
                    stats[mode] = stats.get(mode, 0) + 1
                for _ in done:
                    submit()
        except BaseException:
            for future in pending:
                future.cancel()
            raise
    return etags


def link_or_copy(source, destination):
    """Only share bytes within one immutable release, never across releases."""
    if not stat.S_ISREG(os.lstat(source).st_mode):
        raise ValueError("unsafe image snapshot entry")
    try:
        os.link(source, destination, follow_symlinks=False)
    except OSError as error:
        if error.errno not in {
            errno.EXDEV,
            errno.EPERM,
            errno.EACCES,
            errno.ENOTSUP,
            errno.EMLINK,
        }:
            raise
        shutil.copy2(source, destination)
    return destination


class PublisherConnection:
    """Reuse transport only; every object is rechecked and every request signed."""

    def __init__(self):
        self.client = None
        self.identity = None
        self.created = 0

    def get(self, config, factory):
        import os
        import time
        from pathlib import Path

        def fingerprint(name):
            path = os.environ.get(name)
            if path is None:
                return None
            info = Path(path).stat()
            return (
                path,
                info.st_dev,
                info.st_ino,
                info.st_size,
                info.st_mtime_ns,
                info.st_ctime_ns,
            )

        identity = (
            config["endpoint"],
            config["bucket"],
            *(
                fingerprint(name)
                for name in ("S3_ACCESS_KEY_FILE", "S3_SECRET_KEY_FILE", "S3_CA_FILE")
            ),
        )
        if (
            self.client is None
            or identity != self.identity
            or time.monotonic() - self.created >= 30
        ):
            self.close()
            self.client = factory()
            self.identity = identity
            self.created = time.monotonic()
        return self.client

    def close(self):
        if self.client is not None:
            self.client.close()
            self.client = None
            self.identity = None
