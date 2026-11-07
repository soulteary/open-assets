"""First-run setup must refuse existing or partially initialized deployments."""

import concurrent.futures
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "bootstrap_guard", "scripts/bootstrap_guard.py"
)
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


class BootstrapGuard(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.app = self.root / "app"
        self.app.mkdir()
        self.data = self.root / "data"

    def test_first_run_claim_completion_and_repeat_refusal(self):
        preparation = guard.prepare(self.app, self.data)
        with self.assertRaises(guard.SetupRefused):
            guard.initialize(self.app / "private")
        guard.complete(preparation)
        marker = guard.initialize(self.app / "private")
        guard.complete(marker)
        before = {p.name: p.read_bytes() for p in (self.app / "private").iterdir()}
        with self.assertRaises(guard.SetupRefused):
            guard.prepare(self.app, self.data)
        with self.assertRaises(guard.SetupRefused):
            guard.initialize(self.app / "private")
        self.assertEqual(
            before, {p.name: p.read_bytes() for p in (self.app / "private").iterdir()}
        )

    def test_existing_env_data_legacy_and_partial_state_are_preserved(self):
        (self.app / ".env").write_text("preserve")
        with self.assertRaises(guard.SetupRefused):
            guard.prepare(self.app, self.data)
        self.assertEqual((self.app / ".env").read_text(), "preserve")
        self.assertFalse((self.app / "private").exists())
        (self.app / ".env").unlink()
        self.data.mkdir()
        (self.data / "object").write_text("data")
        with self.assertRaises(guard.SetupRefused):
            guard.prepare(self.app, self.data)
        private = self.app / "private"
        private.mkdir()
        (private / "admin_password").write_text("preserve")
        with self.assertRaises(guard.SetupRefused):
            guard.initialize(private)
        self.assertFalse((private / ".initialization.json").exists())
        self.assertEqual((self.data / "object").read_text(), "data")

    def test_failed_run_and_symlink_markers_cannot_bypass_guard(self):
        marker = guard.prepare(self.app, self.data)
        guard.complete(marker)
        claimed = guard.initialize(self.app / "private")
        with self.assertRaises(guard.SetupRefused):
            guard.initialize(self.app / "private")
        claimed.unlink()
        marker.unlink()
        marker.symlink_to(self.root / "missing")
        with self.assertRaises(guard.SetupRefused):
            guard.initialize(self.app / "private")

    def test_only_one_concurrent_preparation_wins(self):
        def attempt(_):
            try:
                guard.prepare(self.app, self.data)
                return True
            except guard.SetupRefused:
                return False

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            self.assertEqual(sum(pool.map(attempt, range(8))), 1)


if __name__ == "__main__":
    unittest.main()
