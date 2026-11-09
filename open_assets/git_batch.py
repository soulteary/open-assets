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
