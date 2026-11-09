"""Complete logical manifests over immutable, directly addressed source objects."""

import contextlib
import json
import os
import errno
from pathlib import Path
import re
import stat

LIMIT = 8 * 1024**2


def enabled(config):
    return config.get("publication", {}).get("reference_releases", False)


def relative(value):
    if not isinstance(value, str):
        raise ValueError("invalid object path")
    path = Path(value)
    if (
        not path.parts
        or path.is_absolute()
        or path.as_posix() != value
        or any(p.startswith(".") for p in path.parts)
    ):
        raise ValueError("invalid object path")
    return value


def parts(key, name):
    prefix = f"repositories/{key}/releases/"
    if not isinstance(name, str) or not name.startswith(prefix):
        raise ValueError("cross-repository object reference")
    sha, separator, path = name[len(prefix) :].partition("/")
    if not separator or not re.fullmatch(r"[a-f0-9]{40}", sha):
        raise ValueError("invalid object reference version")
    return sha, relative(path)


def source(manifest, item):
    key, sha = manifest["key"], manifest["sha"]
    if not re.fullmatch(r"[a-z][a-z0-9-]{0,47}", key) or not re.fullmatch(
        r"[a-f0-9]{40}", sha
    ):
        raise ValueError("invalid reference manifest identity")
    name = relative(item["path"])
    refs = manifest.get("object_sources", {})
    if not isinstance(refs, dict) or (
        refs and manifest.get("object_layout") != "references-v1"
    ):
        raise ValueError("unsupported object references")
    value = refs.get(
        name,
        dict(
            key=f"repositories/{key}/releases/{sha}/{name}",
            content_type=item["content_type"],
        ),
    )
    if not isinstance(value, dict) or set(value) != {"key", "content_type"}:
        raise ValueError("invalid object reference")
    parts(key, value["key"])
    mime = value["content_type"]
    if (
        not isinstance(mime, str)
        or not 0 < len(mime) <= 256
        or any(ord(c) < 32 or ord(c) > 126 for c in mime)
    ):
        raise ValueError("invalid source content type")
    return value


def validate(manifest):
    files = manifest["files"]
    if not isinstance(files, list) or not 0 < len(files) <= 10000:
        raise ValueError("invalid reference manifest files")
    names = {relative(item["path"]) for item in files}
    if len(names) != len(files) or set(manifest.get("object_sources", {})) - names:
        raise ValueError("unexpected reference manifest entry")
    for item in files:
        mime = item["content_type"]
        if (
            type(item["size"]) is not int
            or item["size"] < 0
            or not isinstance(item["sha256"], str)
            or not re.fullmatch(r"[a-f0-9]{64}", item["sha256"])
            or not isinstance(mime, str)
            or not 0 < len(mime) <= 256
            or any(ord(c) < 32 or ord(c) > 126 for c in mime)
        ):
            raise ValueError("invalid reference file metadata")
    return {item["path"]: source(manifest, item) for item in files}


def read(path):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError as error:
        if error.errno == errno.ELOOP:
            raise ValueError("linked manifest") from None
        raise
    with os.fdopen(fd, "rb") as stream:
        value = os.fstat(stream.fileno())
        if not stat.S_ISREG(value.st_mode) or value.st_size > LIMIT:
            raise ValueError("unsafe manifest")
        raw = stream.read(LIMIT + 1)
    if len(raw) > LIMIT:
        raise ValueError("oversized manifest")
    return json.loads(raw)


def verify_sources(config, client, records):
    """A few paged listings replace individual HEAD/COPY/GET requests."""
    groups = {}
    key = records[0]["key"].split("/")[1] if records else None
    for record in records:
        sha, _ = parts(key, record["key"])
        groups.setdefault(f"repositories/{key}/releases/{sha}/", {})[record["key"]] = (
            record
        )
    from concurrent.futures import ThreadPoolExecutor
    import threading
    from .publication import policy

    ranges = []
    for prefix, expected in groups.items():
        names = sorted(expected)
        for offset in range(0, len(names), 1000):
            selected = names[offset : offset + 1000]
            ranges.append(
                (
                    prefix,
                    names[offset - 1] if offset else None,
                    selected[-1],
                    {name: expected[name] for name in selected},
                )
            )
    if len(ranges) > 128:
        raise ValueError("reference listing budget exceeded")
    pages = 0
    guard = threading.Lock()

    def verify_range(task):
        nonlocal pages
        prefix, lower, upper, expected = task
        found, seen = set(), set()
        token = None
        while True:
            with guard:
                pages += 1
                if pages > 128:
                    raise ValueError("reference listing budget exceeded")
            args = dict(Bucket=config["bucket"], Prefix=prefix, MaxKeys=1000)
            if token:
                args["ContinuationToken"] = token
            elif lower:
                args["StartAfter"] = lower
            page = client.list_objects_v2(**args)
            contents = page.get("Contents", [])
            if (
                not isinstance(contents, list)
                or len(contents) > 1000
                or type(page.get("IsTruncated")) is not bool
            ):
                raise ValueError("invalid reference listing page")
            next_token = page.get("NextContinuationToken")
            if page["IsTruncated"] and (
                not isinstance(next_token, str) or not next_token or next_token in seen
            ):
                raise ValueError("invalid reference pagination")
            reached_upper = False
            for obj in contents:
                name = obj["Key"]
                if not isinstance(name, str) or not name.startswith(prefix):
                    raise ValueError("invalid reference listing key")
                reached_upper |= name >= upper
                if name not in expected:
                    continue
                record = expected[name]
                modified = obj.get("LastModified")
                if (
                    name in found
                    or obj.get("ETag") != record["etag"]
                    or obj.get("Size") != record["item"]["size"]
                    or modified is None
                    or int(modified.timestamp()) != record.get("modified")
                ):
                    raise ValueError("referenced source changed")
                found.add(name)
            if reached_upper or page.get("IsTruncated") is False:
                break
            token = page.get("NextContinuationToken")
            if not isinstance(token, str) or not token or token in seen:
                raise ValueError("invalid reference pagination")
            seen.add(token)
        if found != set(expected):
            raise ValueError("referenced source missing")

    with ThreadPoolExecutor(max_workers=policy(config)) as pool:
        for _ in pool.map(verify_range, ranges):
            pass
    return dict(source_list_requests=pages, source_ranges=len(ranges))


def reference_inventory(config, key, deadline):
    """Conservatively keep source versions referenced by known logical releases."""
    from .retention import check_deadline

    folder = Path(config["state_root"]) / "manifests" / key
    protected = set()
    versions = {}
    if not folder.exists():
        return protected, versions
    if not stat.S_ISDIR(folder.lstat().st_mode):
        raise ValueError("unsafe reference manifest directory")
    for index, path in enumerate(folder.iterdir()):
        check_deadline(deadline)
        if index >= 10000:
            raise ValueError("reference manifest budget exceeded")
        if not re.fullmatch(r"[a-f0-9]{40}\.json", path.name):
            raise ValueError("unknown reference manifest")
        manifest = read(path)
        if manifest.get("object_layout") != "references-v1":
            continue
        if manifest.get("key") != key or manifest.get("sha") != path.stem:
            raise ValueError("reference manifest identity mismatch")
        marker = (
            Path(config["state_root"])
            / "object-retention"
            / key
            / (path.stem + ".json")
        )
        if marker.exists() or marker.is_symlink():
            receipt = read(marker)
            if receipt.get("key") != key or receipt.get("sha") != path.stem:
                raise ValueError("reference receipt identity mismatch")
            if receipt.get("status") == "complete":
                continue
        versions[path.stem] = path.stat().st_mtime
        for value in validate(manifest).values():
            sha, _ = parts(key, value["key"])
            if sha != path.stem:
                protected.add(sha)
    return protected, versions


@contextlib.contextmanager
def read_guard(path, exclusive=False, required=False):
    """Coordinate readers with physical object compaction, using one fixed inode."""
    import fcntl
    import time

    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        if required:
            raise ValueError(
                "upgrade the object reader and install its compaction lock first"
            ) from None
        yield
        return
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise ValueError("unsafe compaction guard")
        deadline = time.monotonic() + (30 if exclusive else 5)
        while True:
            try:
                fcntl.flock(
                    fd, (fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH) | fcntl.LOCK_NB
                )
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.01)
        yield
    finally:
        os.close(fd)
