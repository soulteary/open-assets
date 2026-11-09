"""Fixed-operation Unix broker for the authenticated status viewer.

No shell, request arguments, Docker socket, or host paths are exposed to Info.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import socket
import socketserver
import stat
import struct
import time

from . import telemetry


class Collector:
    def __init__(self, root):
        self.root = root
        self.last = float("-inf")

    def refresh(self):
        if time.monotonic() - self.last < 10:
            return dict(status="cooldown")
        output = (
            Path(telemetry.environment(self.root)["DATA_ROOT"]) / "info/snapshot.json"
        )
        with (output.parent / ".collector.lock").open("a") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return dict(status="busy")
            data = telemetry.collect(self.root, refresh_storage=True)
            telemetry.write_snapshot(output, data)
        self.last = time.monotonic()
        return dict(status="collected", collected_at=data["collected_at"])


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(2)
        try:
            if hasattr(socket, "SO_PEERCRED"):
                _, uid, _ = struct.unpack(
                    "3i",
                    self.request.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12),
                )
                if uid not in (0, 20010):
                    return
            with self.request.makefile("rb") as stream:
                if stream.readline(32) != b"REFRESH\n":
                    return
            result = self.server.collector.refresh()
        except Exception:
            result = dict(status="unavailable")
        try:
            self.request.sendall(json.dumps(result).encode() + b"\n")
        except OSError:
            pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path("/app/open-assets"))
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run the collector broker as root")
    root = args.root.resolve()
    path = Path(telemetry.environment(root)["DATA_ROOT"]) / "info-refresh/control.sock"
    path.parent.mkdir(mode=0o750, parents=True, exist_ok=True)
    os.chown(path.parent, 0, 20010)
    os.chmod(path.parent, 0o750)
    if path.exists() or path.is_symlink():
        if not stat.S_ISSOCK(path.lstat().st_mode):
            raise ValueError("refusing a non-socket broker path")
        path.unlink()
    with socketserver.UnixStreamServer(str(path), Handler) as server:
        os.chown(path, 0, 20010)
        os.chmod(path, 0o660)
        server.collector = Collector(root)
        server.serve_forever()


if __name__ == "__main__":
    main()
