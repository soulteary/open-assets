"""Manual refresh runs only bounded collection, with no web Docker privilege."""

import fcntl
import http.client
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch

from open_assets import info, info_refresh


class Refresh(unittest.TestCase):
    @unittest.skipUnless(
        os.geteuid() in (0, 20010), "broker authenticates the production UID"
    )
    def test_unix_protocol_accepts_only_fixed_refresh_operation(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "control.sock"
            server = info_refresh.socketserver.UnixStreamServer(
                str(path), info_refresh.Handler
            )
            server.collector = Mock()
            server.collector.refresh.return_value = dict(
                status="collected", collected_at=123
            )
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with patch.dict(info.os.environ, INFO_REFRESH_SOCKET=str(path)):
                    self.assertEqual(info.refresh_snapshot()["status"], "collected")
                connection = info_refresh.socket.socket(info_refresh.socket.AF_UNIX)
                connection.connect(str(path))
                connection.sendall(b"RUN /bin/sh\n")
                self.assertEqual(connection.recv(1024), b"")
                connection.close()
                server.collector.refresh.assert_called_once()
            finally:
                server.shutdown()
                server.server_close()
                thread.join()

    def test_collector_cooldown_busy_and_failure_preserve_previous_data(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "info").mkdir()
            snapshot = root / "info/snapshot.json"
            snapshot.write_text("old")
            collector = info_refresh.Collector(root)
            with (
                patch.object(
                    info_refresh.telemetry,
                    "environment",
                    return_value=dict(DATA_ROOT=str(root)),
                ),
                patch.object(
                    info_refresh.telemetry,
                    "collect",
                    return_value=dict(collected_at=123),
                ) as collect,
                patch.object(
                    info_refresh.telemetry,
                    "write_snapshot",
                    side_effect=lambda p, d: p.write_text(json.dumps(d)),
                ),
            ):
                with (root / "info/.collector.lock").open("a") as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    self.assertEqual(collector.refresh()["status"], "busy")
                    self.assertEqual(snapshot.read_text(), "old")
                self.assertEqual(collector.refresh()["status"], "collected")
                self.assertEqual(collector.refresh()["status"], "cooldown")
                collect.assert_called_once_with(root, refresh_storage=True)
                collector.last = float("-inf")
                collect.side_effect = OSError("sensitive private path")
                with self.assertRaises(OSError):
                    collector.refresh()
                self.assertEqual(json.loads(snapshot.read_text())["collected_at"], 123)

    def test_post_requires_same_origin_header_and_never_accepts_arguments(self):
        server = info.http.server.ThreadingHTTPServer(("127.0.0.1", 0), info.Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with patch.object(
                info,
                "refresh_snapshot",
                return_value=dict(status="collected", collected_at=123),
            ) as refresh:
                for headers, status in (
                    ({}, 403),
                    (
                        {
                            "X-Open-Assets-Refresh": "1",
                            "Origin": "https://foreign.invalid",
                        },
                        403,
                    ),
                    ({"X-Open-Assets-Refresh": "1", "Content-Length": "1"}, 403),
                    (
                        {
                            "X-Open-Assets-Refresh": "1",
                            "Host": "info.example",
                            "Origin": "https://info.example",
                        },
                        200,
                    ),
                ):
                    connection = http.client.HTTPConnection(
                        "127.0.0.1", server.server_port
                    )
                    connection.request("POST", "/api/refresh", headers=headers)
                    response = connection.getresponse()
                    self.assertEqual(response.status, status)
                    response.read()
                    connection.close()
                refresh.assert_called_once()
                for value, status in (("busy", 409), ("cooldown", 429)):
                    refresh.return_value = dict(status=value)
                    connection = http.client.HTTPConnection(
                        "127.0.0.1", server.server_port
                    )
                    connection.request(
                        "POST", "/api/refresh", headers={"X-Open-Assets-Refresh": "1"}
                    )
                    response = connection.getresponse()
                    self.assertEqual(response.status, status)
                    self.assertEqual(response.getheader("Cache-Control"), "no-store")
                    response.read()
                    connection.close()
                refresh.side_effect = OSError("never disclose this path")
                connection = http.client.HTTPConnection("127.0.0.1", server.server_port)
                connection.request(
                    "POST", "/api/refresh", headers={"X-Open-Assets-Refresh": "1"}
                )
                response = connection.getresponse()
                self.assertEqual(response.status, 503)
                self.assertEqual(
                    json.loads(response.read()), dict(status="unavailable")
                )
                connection.close()
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
