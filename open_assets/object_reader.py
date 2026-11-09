"""Read-only version URL resolver. No storage credentials or Docker access."""

import contextlib
from collections import OrderedDict
import http.client
import http.server
import os
from pathlib import Path
import re
import ssl
import stat
import threading
import urllib.parse

from . import object_refs
from .svg import CSP

MANIFESTS = Path(os.environ.get("OBJECT_MANIFESTS", "/manifests"))
RECEIPTS = Path(os.environ.get("OBJECT_RECEIPTS", "/receipts"))
BUCKET = os.environ.get("OBJECT_BUCKET", "attachments")
ENDPOINT = os.environ.get("OBJECT_ENDPOINT", "https://storage.internal:9000")
CACHE = OrderedDict()
CACHE_LOCK = threading.Lock()
ACTIVE = threading.BoundedSemaphore(32)


def resolve(path):
    # Decode once, preserving literal '+' and percent characters in Git names.
    name = urllib.parse.unquote(urllib.parse.urlsplit(path).path, errors="strict")
    match = re.fullmatch(
        r"/"
        + re.escape(BUCKET)
        + r"/repositories/([a-z][a-z0-9-]{0,47})/releases/([a-f0-9]{40})/(.+)",
        name,
        re.S,
    )
    if not match:
        raise ValueError("invalid version object URL")
    key, sha, relative = match.groups()
    object_refs.relative(relative)
    filename = MANIFESTS / key / (sha + ".json")
    try:
        value = filename.lstat()
    except FileNotFoundError:
        # Older manually uploaded objects and legacy releases need no migration.
        return f"repositories/{key}/releases/{sha}/{relative}", None
    if not stat.S_ISDIR(filename.parent.lstat().st_mode):
        raise ValueError("unsafe manifest directory")
    identity = (value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns)
    cache_key = (key, sha)
    with CACHE_LOCK:
        entry = CACHE.get(cache_key)
        if entry is None or entry[0] != identity:
            manifest = object_refs.read(filename)
            if manifest.get("key") != key or manifest.get("sha") != sha:
                raise ValueError("manifest identity mismatch")
            refs = object_refs.validate(manifest)
            files = {item["path"]: item for item in manifest["files"]}
            entry = (
                identity,
                refs,
                files,
                manifest.get("object_layout") == "references-v1",
            )
            CACHE[cache_key] = entry
            while (
                len(CACHE) > 8
                or sum(value[0][1] for value in CACHE.values()) > object_refs.LIMIT
            ):
                CACHE.popitem(last=False)
        CACHE.move_to_end(cache_key)
    marker = RECEIPTS / key / (sha + ".json")
    if entry[3] and (marker.exists() or marker.is_symlink()):
        receipt = object_refs.read(marker)
        if receipt.get("key") != key or receipt.get("sha") != sha:
            raise ValueError("invalid object receipt")
        if receipt.get("status") in ("complete", "deleting"):
            raise FileNotFoundError("retired object release")
    item = entry[2].get(relative)
    if item is None:
        if not entry[3]:
            return f"repositories/{key}/releases/{sha}/{relative}", None
        raise FileNotFoundError("object absent from complete manifest")
    return entry[1][relative]["key"], item["content_type"]


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_HEAD(self):
        self.do_GET()

    def error(self, status):
        self.send_response(status)
        if status == 200:
            self.send_header("X-Open-Assets-Reader", "guard-v1")
        self.send_header("Content-Length", "0")
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Security-Policy", CSP)
        self.end_headers()

    def do_GET(self):
        if self.path == "/healthz":
            self.error(200)
            return
        if not ACTIVE.acquire(blocking=False):
            self.error(503)
            return
        connection = None
        headers_sent = False
        guard = contextlib.ExitStack()
        try:
            guard.enter_context(object_refs.read_guard(RECEIPTS / "compaction.lock"))
            key, mime = resolve(self.path)
            endpoint = urllib.parse.urlsplit(ENDPOINT)
            if (
                endpoint.scheme != "https"
                or not endpoint.hostname
                or endpoint.username
                or endpoint.path not in ("", "/")
            ):
                raise ValueError("invalid fixed object endpoint")
            connection = http.client.HTTPSConnection(
                endpoint.hostname,
                endpoint.port or 443,
                context=ssl.create_default_context(
                    cafile=os.environ.get("OBJECT_CA_FILE", "/tls/ca.crt")
                ),
                timeout=20,
            )
            request_headers = {
                h: self.headers[h]
                for h in (
                    "Range",
                    "If-Range",
                    "If-Match",
                    "If-None-Match",
                    "If-Modified-Since",
                    "If-Unmodified-Since",
                )
                if h in self.headers
            }
            connection.request(
                self.command,
                "/" + BUCKET + "/" + urllib.parse.quote(key, safe="/"),
                headers=request_headers,
            )
            response = connection.getresponse()
            self.send_response(response.status)
            for header in (
                "Content-Length",
                "Content-Range",
                "Content-Encoding",
                "ETag",
                "Last-Modified",
                "Accept-Ranges",
                "Cache-Control",
            ):
                value = response.getheader(header)
                if value is not None:
                    self.send_header(header, value)
            self.send_header(
                "Content-Type",
                mime
                if mime and response.status in (200, 206, 304)
                else response.getheader("Content-Type", "application/octet-stream"),
            )
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Content-Security-Policy", CSP)
            self.end_headers()
            headers_sent = True
            if self.command != "HEAD":
                while block := response.read(65536):
                    self.wfile.write(block)
        except FileNotFoundError:
            if not headers_sent:
                self.error(404)
        except (ValueError, UnicodeError, TypeError, KeyError, AttributeError):
            if not headers_sent:
                self.error(400)
        except (OSError, http.client.HTTPException):
            if not headers_sent:
                self.error(502)
            else:
                self.close_connection = True
        finally:
            if connection:
                connection.close()
            guard.close()
            ACTIVE.release()


def main():
    http.server.ThreadingHTTPServer(("0.0.0.0", 8083), Handler).serve_forever()


if __name__ == "__main__":
    main()
