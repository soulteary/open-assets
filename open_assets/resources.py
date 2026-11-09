"""Admission budgets and bounded Git I/O. These are not filesystem quotas."""

import contextlib
import hashlib
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time

DEFAULTS = {
    "git_repository_bytes": 2 * 1024**3,
    "git_total_bytes": 8 * 1024**3,
    "git_fetch_bytes": 512 * 1024**2,
    "git_min_free_bytes": 512 * 1024**2,
    "git_timeout_seconds": 120,
    "git_process_memory_bytes": 512 * 1024**2,
    "git_output_bytes": 8 * 1024**2,
    "scan_entries": 200000,
    "queue_active_records": 1000,
    "queue_total_records": 10000,
    "queue_reserved_bytes": 1024**3,
}


class ResourceLimit(RuntimeError):
    """A finite diagnostic code, never a remote URL or subprocess stderr."""


def policy(c):
    supplied = c.get("resource_limits", {})
    if not isinstance(supplied, dict) or set(supplied) - set(DEFAULTS):
        raise ValueError("invalid resource_limits")
    result = dict(DEFAULTS, **supplied)
    if any(type(v) is not int or v <= 0 for v in result.values()):
        raise ValueError("resource limits must be positive integers")
    if result["queue_active_records"] > result["queue_total_records"]:
        raise ValueError("active queue limit exceeds total limit")
    return result


def usage(root, limit, check=None):
    """Count logical bytes without following symlinks or opening file contents."""
    total = count = 0
    stack = [Path(root)]
    while stack:
        path = stack.pop()
        if not path.exists() and not path.is_symlink():
            continue
        if not stat.S_ISDIR(path.lstat().st_mode):
            raise ResourceLimit("unsafe_resource_entry")
        with os.scandir(path) as entries:
            for entry in entries:
                if check:
                    check()
                count += 1
                if count > limit:
                    raise ResourceLimit("scan_entries")
                try:
                    info = entry.stat(follow_symlinks=False)
                except FileNotFoundError:
                    continue  # Git renames temporary objects while a fetch is running.
                if stat.S_ISDIR(info.st_mode):
                    stack.append(Path(entry.path))
                elif stat.S_ISREG(info.st_mode):
                    total += info.st_size
                else:
                    raise ResourceLimit("unsafe_resource_entry")
    return total, count


def queue_usage(c, check=None):
    """Use the same conservative accounting for admission and observation."""
    p = policy(c)
    root = Path(c["state_root"])
    total = reserved = active = scanned = actual = 0
    locations = {}
    for name in ("jobs", "job-archive", "quarantine"):
        locations[name] = 0
        folder = root / name
        if not folder.exists() and not folder.is_symlink():
            continue
        if not stat.S_ISDIR(folder.lstat().st_mode):
            raise ResourceLimit("scan_entries")
        stack = [folder]
        while stack:
            with os.scandir(stack.pop()) as entries:
                for entry in entries:
                    if check:
                        check()
                    scanned += 1
                    if scanned > p["scan_entries"]:
                        raise ResourceLimit("scan_entries")
                    info = entry.stat(follow_symlinks=False)
                    if stat.S_ISDIR(info.st_mode):
                        stack.append(Path(entry.path))
                    else:
                        # Bad links/FIFOs are counted without following or opening
                        # them. Worker quarantine can then isolate them normally.
                        total += 1
                        locations[name] += 1
                        actual += info.st_size
                        active += name == "jobs"
                        reserved += max(65536, info.st_size)
    return dict(
        active_records=active,
        total_records=total,
        reserved_bytes=reserved,
        bytes=actual,
        locations=locations,
    )


def admit_queue(c):
    """Caller holds queue lock; never delete existing records to admit work."""
    p = policy(c)
    value = queue_usage(c)
    active, total, reserved = (
        value[k] for k in ("active_records", "total_records", "reserved_bytes")
    )
    if active + 1 > p["queue_active_records"]:
        raise ResourceLimit("queue_active_records")
    if total + 1 > p["queue_total_records"]:
        raise ResourceLimit("queue_total_records")
    if reserved + 65536 > p["queue_reserved_bytes"]:
        raise ResourceLimit("queue_reserved_bytes")


def run_git(path, args, c=None, output=None, check=None, monitor=None, input_data=None):
    """Stream stdout into a file; bound time, output, and each Git process.

    A separate exec helper applies rlimits safely when the caller has threads.
    stderr is discarded rather than retaining potentially sensitive remote data.
    """
    p = policy(c or {})
    started = time.monotonic()
    if input_data is not None and (
        not isinstance(input_data, bytes) or len(input_data) > 65536
    ):
        raise ValueError("invalid Git control input")
    with contextlib.ExitStack() as stack:
        captured = stack.enter_context(tempfile.TemporaryFile())
        incoming = None
        if input_data is not None:
            incoming = stack.enter_context(tempfile.TemporaryFile())
            incoming.write(input_data)
            incoming.seek(0)
        target = output if output is not None else captured
        command = [
            sys.executable,
            "-m",
            "open_assets.git_process",
            str(p["git_process_memory_bytes"]),
            str(p["git_fetch_bytes"]),
            "--git-dir",
            str(path),
            *args,
        ]
        process = subprocess.Popen(
            command,
            stdin=incoming,
            stdout=target,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            while True:
                if check:
                    check()
                if time.monotonic() - started > p["git_timeout_seconds"]:
                    raise ResourceLimit("git_timeout_seconds")
                if (
                    output is None
                    and os.fstat(captured.fileno()).st_size > p["git_output_bytes"]
                ):
                    raise ResourceLimit("git_output_bytes")
                if monitor:
                    monitor()
                if process.poll() is not None:
                    break
                threading.Event().wait(0.02)
            if process.returncode:
                raise subprocess.CalledProcessError(
                    process.returncode, ["git", args[0]]
                )
            if output is None:
                captured.seek(0)
                return captured.read(p["git_output_bytes"] + 1)
        finally:
            # Also stop descendants if SSH survives its parent or a limit trips.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except PermissionError:
                # Some macOS development sandboxes permit only direct-child kill.
                # Production Linux uses process groups, including SSH children.
                process.kill()
            finally:
                process.wait()


def fetch(c, repository, path, check=None):
    """Fetch into a temporary ref; only promote after budget checks succeed.

    Failed fetches may leave unreachable Git objects. They still count toward
    future budgets; do not automatically prune history needed by recovery.
    """
    import shutil
    import uuid

    path = Path(path)
    p = policy(c)
    root = Path(c["state_root"]) / "git"
    initial, _ = usage(path, p["scan_entries"])
    aggregate, _ = usage(root, p["scan_entries"])

    def monitor():
        current, _ = usage(path, p["scan_entries"])
        if current > p["git_repository_bytes"]:
            raise ResourceLimit("git_repository_bytes")
        if current - initial > p["git_fetch_bytes"]:
            raise ResourceLimit("git_fetch_bytes")
        if aggregate + current - initial > p["git_total_bytes"]:
            raise ResourceLimit("git_total_bytes")
        if shutil.disk_usage(root).free < p["git_min_free_bytes"]:
            raise ResourceLimit("git_min_free_bytes")

    temporary = "refs/open-assets/fetch-" + uuid.uuid4().hex
    monitor()
    promoted = False
    try:
        run_git(
            path,
            [
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "-c",
                "pack.threads=1",
                "-c",
                "core.bigFileThreshold=1m",
                "-c",
                "pack.windowMemory=16m",
                "-c",
                "pack.deltaCacheSize=16m",
                "-c",
                "core.hooksPath=/dev/null",
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                repository["remote"],
                "+refs/heads/" + repository["branch"] + ":" + temporary,
            ],
            c,
            check=check,
            monitor=monitor,
        )
        # A unique fetch ref is normally loose. Read only a bounded regular
        # producer-written ref, falling back to Git for a concurrently packed ref.
        import re

        try:
            fd = os.open(path / temporary, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        except FileNotFoundError:
            sha = run_git(path, ["rev-parse", temporary], c).decode().strip()
        else:
            with os.fdopen(fd, "rb") as stream:
                if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                    raise ValueError("unsafe fetched Git ref")
                raw = stream.read(42)
            if not re.fullmatch(rb"[a-f0-9]{40}\n", raw):
                raise ValueError("invalid fetched Git ref")
            sha = raw[:40].decode()
        if not re.fullmatch(r"[a-f0-9]{40}", sha):
            raise ValueError("invalid fetched Git commit")
        monitor()
        transaction = (
            f"update refs/heads/source {sha}\ndelete {temporary} {sha}\n".encode()
        )
        run_git(path, ["update-ref", "--stdin"], c, input_data=transaction)
        promoted = True
    finally:
        if not promoted:
            with contextlib.suppress(Exception):
                run_git(path, ["update-ref", "-d", temporary], c)


def remote_head(c, repository, path):
    """Recheck the advertised branch without downloading another object pack."""
    ref = "refs/heads/" + repository["branch"]
    value = run_git(
        path, ["ls-remote", "--exit-code", "--refs", repository["remote"], ref], c
    )
    import re

    match = re.fullmatch(rb"([a-f0-9]{40})\t" + re.escape(ref.encode()) + rb"\n", value)
    if not match:
        raise ValueError("invalid remote branch advertisement")
    return match[1].decode()


def verify_body(body, item):
    """Read at most the expected length plus one byte; always close the body."""
    digest = hashlib.sha256()
    md5 = hashlib.md5(usedforsecurity=False)
    size = 0
    try:
        while True:
            block = body.read(min(1024**2, item["size"] - size + 1))
            if not block:
                break
            size += len(block)
            if size > item["size"]:
                raise ValueError("object size mismatch")
            digest.update(block)
            md5.update(block)
        if size != item["size"] or digest.hexdigest() != item["sha256"]:
            raise ValueError("object verification failed")
        return '"' + md5.hexdigest() + '"'
    finally:
        body.close()
