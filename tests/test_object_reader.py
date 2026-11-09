"""Unchanged version URLs, streaming Range/HEAD/304 and fixed upstream scope."""

import hashlib
import http.client
import io
import json
from pathlib import Path
import tempfile
import threading
import unittest
import urllib.parse
from unittest.mock import patch

from open_assets import object_reader as reader


class Response:
    def __init__(self, status, data, headers):
        self.status = status
        self.body = io.BytesIO(data)
        self.headers = headers

    def read(self, size):
        return self.body.read(size)

    def getheader(self, name, default=None):
        return self.headers.get(name, default)


class Connection:
    calls = []
    body = b'<svg xmlns="http://www.w3.org/2000/svg"/>'

    def __init__(self, host, port, **kwargs):
        self.host = host
        self.port = port

    def request(self, method, path, headers):
        self.calls.append((self.host, self.port, method, path, headers))
        self.method = method
        self.headers = headers

    def getresponse(self):
        status = (
            304
            if self.headers.get("If-None-Match") == '"etag"'
            else 206
            if "Range" in self.headers
            else 200
        )
        body = self.body[:2] if status == 206 else b"" if status == 304 else self.body
        headers = {
            "Content-Length": str(len(body)),
            "Content-Type": "application/xml",
            "ETag": '"etag"',
            "Cache-Control": "public, max-age=31536000, immutable",
            "Accept-Ranges": "bytes",
        }
        if status == 206:
            headers["Content-Range"] = f"bytes 0-1/{len(self.body)}"
        return Response(status, body, headers)

    def close(self):
        pass


class Reader(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.sha = "b" * 40
        self.origin = "a" * 40
        self.path = "images/中文 +%.svg"
        self.manifest = dict(
            key="assets",
            sha=self.sha,
            object_layout="references-v1",
            files=[
                dict(
                    path=self.path,
                    size=len(Connection.body),
                    sha256=hashlib.sha256(Connection.body).hexdigest(),
                    content_type="image/svg+xml",
                )
            ],
            object_sources={
                self.path: dict(
                    key=f"repositories/assets/releases/{self.origin}/images/original.svg",
                    content_type="application/xml",
                )
            },
        )
        self.filename = self.root / "assets" / (self.sha + ".json")
        self.filename.parent.mkdir()
        self.filename.write_text(json.dumps(self.manifest))
        self.patches = [
            patch.object(reader, "MANIFESTS", self.root),
            patch.object(reader, "RECEIPTS", self.root / "receipts"),
            patch.object(reader, "ENDPOINT", "https://storage.internal:9000"),
            patch.object(reader.http.client, "HTTPSConnection", Connection),
            patch.object(reader.ssl, "create_default_context", return_value=None),
        ]
        for p in self.patches:
            p.start()
            self.addCleanup(p.stop)
        reader.CACHE.clear()
        Connection.calls = []
        self.server = reader.http.server.ThreadingHTTPServer(
            ("127.0.0.1", 0), reader.Handler
        )
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.stop)

    def stop(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def request(self, path=None, method="GET", headers=None):
        url = (
            path
            or f"/attachments/repositories/assets/releases/{self.sha}/"
            + urllib.parse.quote(self.path, safe="/")
        )
        c = http.client.HTTPConnection("127.0.0.1", self.server.server_port)
        c.request(method, url, headers=headers or {})
        r = c.getresponse()
        result = r.status, dict(r.getheaders()), r.read()
        c.close()
        return result

    def test_same_url_resolves_direct_object_with_mime_and_conditional_semantics(self):
        status, headers, body = self.request(
            headers={"Authorization": "do not forward", "Cookie": "do not forward"}
        )
        self.assertEqual(status, 200)
        self.assertEqual(body, Connection.body)
        self.assertEqual(headers["Content-Type"], "image/svg+xml")
        call = Connection.calls[-1]
        self.assertEqual(call[0], "storage.internal")
        self.assertIn(self.origin, call[3])
        self.assertEqual(call[4], {})
        status, headers, body = self.request(headers={"Range": "bytes=0-1"})
        self.assertEqual(status, 206)
        self.assertEqual(body, Connection.body[:2])
        self.assertIn("Content-Range", headers)
        status, headers, body = self.request(method="HEAD")
        self.assertEqual(status, 200)
        self.assertEqual(body, b"")
        self.assertEqual(int(headers["Content-Length"]), len(Connection.body))
        self.assertEqual(self.request(headers={"If-None-Match": '"etag"'})[0], 304)

    def test_legacy_url_and_absent_manifest_still_use_original_object_key(self):
        self.filename.unlink()
        self.assertEqual(self.request()[0], 200)
        self.assertIn(self.sha, Connection.calls[-1][3])

    def test_cross_project_reference_bad_path_and_unknown_file_never_fetch_backend(
        self,
    ):
        for path in (
            "/attachments/repositories/assets/releases/" + self.sha + "/%2e%2e/secret",
            "/other/private",
            f"/attachments/repositories/assets/releases/{self.sha}/unknown.txt",
        ):
            self.assertIn(self.request(path)[0], (400, 404))
        self.manifest["object_sources"][self.path]["key"] = (
            f"repositories/foreign/releases/{self.origin}/secret"
        )
        self.filename.write_text(json.dumps(self.manifest))
        self.assertEqual(self.request()[0], 400)
        self.assertEqual(Connection.calls, [])

    def test_retired_reference_release_and_linked_manifest_are_not_served(self):
        marker = self.root / "receipts/assets" / (self.sha + ".json")
        marker.parent.mkdir(parents=True)
        marker.write_text(
            json.dumps(dict(key="assets", sha=self.sha, status="complete"))
        )
        self.assertEqual(self.request()[0], 404)
        self.filename.unlink()
        self.filename.symlink_to(marker)
        self.assertEqual(self.request()[0], 400)
        self.assertEqual(Connection.calls, [])
