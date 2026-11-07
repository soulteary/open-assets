"""Claim first-run setup before any credentials or remote state are changed."""

import json
import os
from pathlib import Path
import stat
import time


class SetupRefused(RuntimeError):
    pass


def present(path):
    return path.exists() or path.is_symlink()


def write_new(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as stream:
        json.dump(value, stream)
        stream.flush()
        os.fsync(stream.fileno())
    sync(path.parent)


def sync(folder):
    fd = os.open(folder, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def unused(app, data):
    app, data = Path(app), Path(data)
    private = app / "private"
    if present(app / ".env") or present(private):
        raise SetupRefused(
            "existing private configuration; use normal maintenance commands"
        )
    if present(data) and (
        data.is_symlink() or not data.is_dir() or any(data.iterdir())
    ):
        raise SetupRefused(
            "existing application data; preparation requires an unused deployment"
        )


def prepare(app, data):
    app, data = Path(app), Path(data)
    unused(app, data)
    private = app / "private"
    try:
        private.mkdir(mode=0o700)
    except FileExistsError:
        raise SetupRefused(
            "another preparation already claimed this deployment"
        ) from None
    marker = private / ".preparation.json"
    write_new(marker, dict(schema_version=1, status="running", started_at=time.time()))
    return marker


def initialize(private):
    private = Path(private)
    marker = private / ".initialization.json"
    if present(marker) or present(private / "oc-cli"):
        raise SetupRefused(
            "initialization already started; inspect existing state before recovery"
        )
    try:
        fd = os.open(
            private / ".preparation.json", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
        )
        with os.fdopen(fd, "rb") as stream:
            if not stat.S_ISREG(os.fstat(fd).st_mode):
                raise ValueError()
            value = json.loads(stream.read(4097))
        if value.get("schema_version") != 1 or value.get("status") != "complete":
            raise ValueError()
    except (OSError, ValueError, AttributeError):
        raise SetupRefused(
            "fresh completed preparation is required; existing deployments must not be initialized"
        ) from None
    try:
        write_new(
            marker, dict(schema_version=1, status="running", started_at=time.time())
        )
    except FileExistsError:
        raise SetupRefused(
            "another initialization already claimed this deployment"
        ) from None
    return marker


def complete(marker):
    # A failed or interrupted setup retains its running marker; do not silently retry.
    temporary = marker.with_suffix(".complete.tmp")
    write_new(
        temporary, dict(schema_version=1, status="complete", completed_at=time.time())
    )
    os.replace(temporary, marker)
    sync(marker.parent)
