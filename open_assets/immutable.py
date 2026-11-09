"""Proofs for producer-owned, read-only files; never write through reused inodes."""

import os
import stat
import errno
import hashlib
import shutil
import time

MAX_PROOF_AGE = 3600


def fingerprint(path):
    value = path.lstat()
    if not stat.S_ISREG(value.st_mode):
        raise ValueError("unsafe immutable file")
    return [
        value.st_dev,
        value.st_ino,
        value.st_size,
        value.st_mtime_ns,
        value.st_ctime_ns,
        stat.S_IMODE(value.st_mode),
    ]


def matches(path, item, proof, allow_expired=False):
    return (
        isinstance(proof, dict)
        and proof.get("sha256") == item["sha256"]
        and proof.get("size") == item["size"]
        and type(proof.get("verified_at")) in (int, float)
        and 0 <= time.time() - proof["verified_at"]
        and (allow_expired or time.time() - proof["verified_at"] <= MAX_PROOF_AGE)
        and fingerprint(path) == proof.get("fingerprint")
        and not (proof["fingerprint"][-1] & 0o222)
    )


def record(path, item):
    # Called only after the producer hashes the complete file and fsyncs it.
    os.chmod(path, 0o444)
    with path.open("rb") as stream:
        os.fsync(stream.fileno())
    return dict(
        size=item["size"],
        sha256=item["sha256"],
        fingerprint=fingerprint(path),
        verified_at=time.time(),
    )


def link(source, destination, proof):
    if not matches(source, proof, proof):
        raise ValueError("immutable source identity changed")
    before = source.lstat()
    try:
        os.link(source, destination, follow_symlinks=False)
    except OSError as error:
        if error.errno not in (
            errno.EXDEV,
            errno.EPERM,
            errno.EACCES,
            errno.ENOTSUP,
            errno.EMLINK,
        ):
            raise
        shutil.copyfile(source, destination)
        digest = hashlib.sha256()
        with destination.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1048576), b""):
                digest.update(chunk)
        if digest.hexdigest() != proof["sha256"] or not matches(source, proof, proof):
            raise ValueError("immutable fallback copy differs")
        return record(destination, proof)
    after = source.lstat()
    current = fingerprint(source)
    previous = proof["fingerprint"]
    if (
        current[:4] != previous[:4]
        or current[5:] != previous[5:]
        or after.st_nlink != before.st_nlink + 1
    ):
        raise ValueError("immutable source changed while linking")
    # Only our own hardlink operation may refresh ctime. Shared proof objects
    # keep all views of this inode consistent, without accepting new bytes.
    proof["fingerprint"][:] = current
    return proof
