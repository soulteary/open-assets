"""Bounded object transfers and local immutable snapshot image views."""

from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import errno
import os
import shutil
import stat

from .resources import verify_body


def policy(config):
    value = config.get("publication", {})
    if not isinstance(value, dict) or set(value) - {"object_workers"}:
        raise ValueError("invalid publication settings")
    workers = value.get("object_workers", 4)
    if type(workers) is not int or not 1 <= workers <= 8:
        raise ValueError("publication.object_workers must be between 1 and 8")
    return workers


def transfer(client, bucket, prefix, staging, item):
    key = prefix + item["path"]
    try:
        existing = client.get_object(Bucket=bucket, Key=key)
    except client.exceptions.NoSuchKey:
        existing = None
    if existing is not None:
        verify_body(existing["Body"], item)
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
        verify_body(client.get_object(Bucket=bucket, Key=key)["Body"], item)


def upload(config, client, prefix, staging, files):
    """At most N active futures; drain running work before exposing failure."""
    workers = policy(config)
    iterator = iter(files)
    pending = set()
    with ThreadPoolExecutor(max_workers=workers) as pool:

        def submit():
            item = next(iterator, None)
            if item is not None:
                pending.add(
                    pool.submit(
                        transfer, client, config["bucket"], prefix, staging, item
                    )
                )

        try:
            for _ in range(workers):
                submit()
            while pending:
                done, pending = wait(pending, return_when=FIRST_COMPLETED)
                # Observe every completed future before submitting more work.
                for future in done:
                    future.result()
                for _ in done:
                    submit()
        except BaseException:
            for future in pending:
                future.cancel()
            raise


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
