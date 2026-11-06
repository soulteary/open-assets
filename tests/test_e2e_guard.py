"""The opt-in check must run before a fresh test can inspect or change a stack."""

import os
from pathlib import Path
import subprocess
import sys
import unittest


class FreshEnvironmentGuards(unittest.TestCase):
    def test_shell_runner_refuses_without_opt_in_even_without_docker(self):
        script = Path("tests/e2e/fresh-stack.sh").resolve()
        result = subprocess.run(
            ["/bin/sh", str(script)], env={"PATH": ""}, capture_output=True, text=True
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("Set OPEN_ASSETS_FRESH_TEST=1", result.stderr)

    def test_verifier_refuses_before_reading_private_configuration(self):
        env = os.environ.copy()
        env.pop("OPEN_ASSETS_FRESH_TEST", None)
        result = subprocess.run(
            [sys.executable, "tests/e2e/verify.py"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires explicit disposable-environment opt-in", result.stderr)


if __name__ == "__main__":
    unittest.main()
