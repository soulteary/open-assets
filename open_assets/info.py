"""Read-only status viewer. Host collection is separate and needs no web access."""

import http.server
import json
import math
import os
import socket
import time
import urllib.parse
from pathlib import Path

STATIC = Path(__file__).parent / "static"
SNAPSHOT = Path(os.environ.get("INFO_SNAPSHOT", "/info/snapshot.json"))


def refresh_snapshot():
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(30)
        connection.connect(
            os.environ.get("INFO_REFRESH_SOCKET", "/run/info-refresh/control.sock")
        )
        connection.sendall(b"REFRESH\n")
        with connection.makefile("rb") as stream:
            value = json.loads(stream.readline(1025))
    if not isinstance(value, dict) or value.get("status") not in (
        "collected",
        "busy",
        "cooldown",
    ):
        raise ValueError("status collection unavailable")
    return value


def read_snapshot(path=None):
    path = path or SNAPSHOT
    if path.stat().st_size > 2 * 1024 * 1024:
        raise ValueError("snapshot exceeds limit")
    snapshot = json.loads(path.read_text())
    if not isinstance(snapshot, dict) or snapshot.get("schema_version") != 1:
        raise ValueError("unsupported snapshot")
    collected = snapshot.get("collected_at")
    if (
        type(collected) not in (int, float)
        or not math.isfinite(collected)
        or not 0 <= collected <= time.time() + 300
    ):
        raise ValueError("invalid collection time")
    snapshot["age_seconds"] = max(0, round(time.time() - snapshot["collected_at"]))
    snapshot["stale"] = snapshot["age_seconds"] > 120
    return snapshot


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_HEAD(self):
        self.do_GET()

    def do_POST(self):
        if self.path != "/api/refresh":
            self.send_error(501)
            return
        origin = self.headers.get("Origin")
        if (
            self.headers.get("X-Open-Assets-Refresh") != "1"
            or (
                origin is not None
                and origin != "https://" + self.headers.get("Host", "")
            )
            or self.headers.get("Content-Length", "0") != "0"
            or self.headers.get("Transfer-Encoding") is not None
        ):
            self.send_error(403)
            return
        try:
            result = refresh_snapshot()
            status = {"collected": 200, "busy": 409, "cooldown": 429}[result["status"]]
        except (OSError, ValueError, TypeError):
            status, result = 503, dict(status="unavailable")
        data = json.dumps(result).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        route = self.path.split("?", 1)[0]
        status = 200
        if route == "/session":
            target = os.environ.get("INFO_AUTH_URL", "")
            parsed = urllib.parse.urlsplit(target)
            if (
                parsed.scheme != "https"
                or not parsed.hostname
                or parsed.path not in ("", "/")
                or parsed.query
                or parsed.fragment
                or parsed.username
                or any(ord(c) < 32 for c in target)
            ):
                status, data, mime = 503, b"Session entry unavailable", "text/plain"
            else:
                self.send_response(303)
                self.send_header("Location", target)
                self.send_header("Cache-Control", "no-store")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
        elif route == "/healthz":
            data, mime = b'{"status":"running"}', "application/json"
        elif route == "/api/status":
            try:
                data = json.dumps(read_snapshot(), ensure_ascii=False).encode()
            except (OSError, ValueError, KeyError, TypeError):
                status, data = 503, b'{"error":"snapshot unavailable"}'
            mime = "application/json"
        elif route in ("/", "/app.js", "/style.css"):
            filename, mime = {
                "/": ("index.html", "text/html"),
                "/app.js": ("app.js", "application/javascript"),
                "/style.css": ("style.css", "text/css"),
            }[route]
            data = (STATIC / filename).read_bytes()
        else:
            status, data, mime = 404, b"Not found", "text/plain"
        self.send_response(status)
        self.send_header("Content-Type", mime + "; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header(
            "Content-Security-Policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; "
            "base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        )
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)


def main():
    server = http.server.ThreadingHTTPServer(("0.0.0.0", 8082), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
