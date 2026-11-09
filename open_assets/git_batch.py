"""Read raw blobs through one bounded cat-file process; never run Git filters."""

import contextlib
import os
import re
import selectors
import signal
import subprocess
import sys
import time

from .resources import ResourceLimit, policy


class GitBatch:
    def __init__(self, path, config, check=None):
        self.limits = policy(config)
        self.path = path
        self.check = check
        self.buffer = bytearray()

    def __enter__(self):
        self.process = subprocess.Popen(
            [
                sys.executable,
                "-m",
                "open_assets.git_process",
                str(self.limits["git_process_memory_bytes"]),
                str(self.limits["git_fetch_bytes"]),
                "--git-dir",
                str(self.path),
                "cat-file",
                "--batch",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            bufsize=0,
            start_new_session=True,
        )
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)
        return self

    def _receive(self, deadline):
        while True:
            if self.check:
                self.check()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ResourceLimit("git_timeout_seconds")
            if self.selector.select(min(remaining, 0.1)):
                chunk = os.read(self.process.stdout.fileno(), 1024**2)
                if not chunk:
                    raise ValueError("truncated Git batch output")
                self.buffer.extend(chunk)
                return

    def _exact(self, count, deadline):
        while len(self.buffer) < count:
            self._receive(deadline)
        result = bytes(self.buffer[:count])
        del self.buffer[:count]
        return result

    def write_blob(self, oid, size, stream):
        if not re.fullmatch(rb"(?:[0-9a-f]{40}|[0-9a-f]{64})", oid):
            raise ValueError("invalid Git object ID")
        deadline = time.monotonic() + self.limits["git_timeout_seconds"]
        self.process.stdin.write(oid + b"\n")
        while b"\n" not in self.buffer:
            if len(self.buffer) > 128:
                raise ValueError("invalid Git batch header")
            self._receive(deadline)
        header, _, rest = self.buffer.partition(b"\n")
        self.buffer = bytearray(rest)
        if header != oid + b" blob " + str(size).encode():
            raise ValueError("Git blob identity/type/size mismatch")
        remaining = size
        while remaining:
            if self.check:
                self.check()
            chunk = self._exact(min(remaining, 1024**2), deadline)
            stream.write(chunk)
            remaining -= len(chunk)
        if self._exact(1, deadline) != b"\n":
            raise ValueError("invalid Git batch delimiter")

    def __exit__(self, *exc):
        try:
            with contextlib.suppress(OSError):
                self.process.stdin.close()
            if exc[0] is None:
                deadline = time.monotonic() + self.limits["git_timeout_seconds"]
                while self.process.poll() is None:
                    if self.check:
                        self.check()
                    if time.monotonic() >= deadline:
                        raise ResourceLimit("git_timeout_seconds")
                    time.sleep(0.01)
                if self.process.returncode:
                    raise subprocess.CalledProcessError(
                        self.process.returncode, ["git", "cat-file", "--batch"]
                    )
        finally:
            self.selector.close()
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except PermissionError:
                self.process.kill()
            self.process.stdout.close()
            self.process.wait()


def tree_entries(path, config, sha, directory, sources, check=None, max_files=10000):
    """Avoid resolving sizes of already proven Git OIDs; bound new metadata reads."""
    from .resources import run_git

    if not sources:
        return run_git(
            path, ["ls-tree", "-rlz", sha, "--", directory], config, check=check
        ).split(b"\0")
    entries = run_git(
        path, ["ls-tree", "-rz", sha, "--", directory], config, check=check
    ).split(b"\0")
    parsed, sizes, missing = [], {}, set()
    for entry in entries:
        if not entry:
            continue
        meta, name = entry.split(b"\t", 1)
        mode, kind, oid = meta.split()
        if (
            mode not in (b"100644", b"100755")
            or kind != b"blob"
            or not re.fullmatch(rb"[0-9a-f]{40}", oid)
        ):
            raise ValueError("unsupported Git entry")
        record = sources.get(oid.decode())
        if record is not None:
            sizes[oid] = record["item"]["size"]
        else:
            missing.add(oid)
        if len(parsed) >= max_files:
            raise ValueError("publication limit exceeded")
        parsed.append((meta, name, oid))
    missing = sorted(missing)
    for offset in range(0, len(missing), 1024):
        requested = missing[offset : offset + 1024]
        output = run_git(
            path,
            ["cat-file", "--batch-check"],
            config,
            check=check,
            input_data=b"\n".join(requested) + b"\n",
        ).splitlines()
        if len(output) != len(requested):
            raise ValueError("incomplete Git blob metadata")
        for expected, line in zip(requested, output):
            parts = line.split()
            if (
                len(parts) != 3
                or parts[0] != expected
                or parts[1] != b"blob"
                or not parts[2].isdigit()
            ):
                raise ValueError("invalid Git blob metadata")
            sizes[expected] = int(parts[2])
    return [
        meta + b" " + str(sizes[oid]).encode() + b"\t" + name
        for meta, name, oid in parsed
    ]
