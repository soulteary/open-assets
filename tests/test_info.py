import http.client
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from open_assets import info, telemetry
from open_assets.benchmark import percentile, summarize_resources


class Info(unittest.TestCase):
    def test_stale_snapshot_and_invalid_schema(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "snapshot.json"
            path.write_text(
                json.dumps(dict(schema_version=1, collected_at=time.time() - 180))
            )
            self.assertTrue(info.read_snapshot(path)["stale"])
            path.write_text('{"schema_version":2}')
            with self.assertRaises(ValueError):
                info.read_snapshot(path)

    def test_future_and_boolean_timestamps_do_not_look_fresh(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "snapshot.json"
            for value in (time.time() + 301, True, -1):
                path.write_text(json.dumps(dict(schema_version=1, collected_at=value)))
                with self.assertRaises(ValueError):
                    info.read_snapshot(path)

    def test_only_known_read_routes_and_no_cache(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "snapshot.json"
            path.write_text(
                json.dumps(dict(schema_version=1, collected_at=time.time()))
            )
            server = info.http.server.ThreadingHTTPServer(
                ("127.0.0.1", 0), info.Handler
            )
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with patch.object(info, "SNAPSHOT", path):
                    conn = http.client.HTTPConnection("127.0.0.1", server.server_port)
                    conn.request("GET", "/api/status")
                    response = conn.getresponse()
                    self.assertEqual(response.status, 200)
                    self.assertEqual(response.getheader("Cache-Control"), "no-store")
                    self.assertIn(
                        "frame-ancestors 'none'",
                        response.getheader("Content-Security-Policy"),
                    )
                    self.assertFalse(json.loads(response.read())["stale"])
                    conn.request("GET", "/../../private/.env")
                    response = conn.getresponse()
                    self.assertEqual(response.status, 404)
                    response.read()
                    conn.request("POST", "/api/status")
                    response = conn.getresponse()
                    self.assertEqual(response.status, 501)
                    response.read()
                    path.unlink()
                    conn.request("GET", "/api/status")
                    response = conn.getresponse()
                    self.assertEqual(response.status, 503)
                    self.assertEqual(
                        json.loads(response.read()), {"error": "snapshot unavailable"}
                    )
                    conn.close()
            finally:
                server.shutdown()
                server.server_close()
                thread.join()

    def test_session_entry_uses_only_configured_https_origin(self):
        server = info.http.server.ThreadingHTTPServer(("127.0.0.1", 0), info.Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with patch.dict(
                info.os.environ,
                {"INFO_AUTH_URL": "https://auth-node-name.console.lab.dev"},
            ):
                conn = http.client.HTTPConnection("127.0.0.1", server.server_port)
                conn.request("GET", "/session?next=https://untrusted.invalid")
                response = conn.getresponse()
                self.assertEqual(response.status, 303)
                self.assertEqual(
                    response.getheader("Location"),
                    "https://auth-node-name.console.lab.dev",
                )
                response.read()
                conn.close()
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_metric_units_and_percentiles(self):
        self.assertEqual(telemetry.size_bytes("1.5MiB"), 1572864)
        self.assertEqual(percentile(list(range(1, 101)), 0.95), 95)
        samples = [
            dict(components=[dict(service="nginx", cpu_percent=2, memory_bytes=10)]),
            dict(components=[dict(service="nginx", cpu_percent=4, memory_bytes=20)]),
        ]
        self.assertEqual(summarize_resources(samples)[0]["memory_mean_bytes"], 15)
