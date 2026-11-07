"""Run the published first-commit/update recipe against a disposable empty remote."""

from pathlib import Path
import re
import subprocess
import tempfile
import unittest


class FirstPublication(unittest.TestCase):
    def test_documented_first_commit_then_update(self):
        text = Path("docs/usage.md").read_text()
        blocks = re.findall(r"```sh\n(.*?)```", text, re.S)
        first = next(b for b in blocks if b.startswith("# 首次发布（空仓库）"))
        update = next(b for b in blocks if b.startswith("# 后续更新"))
        for branch in ("main", "master"):
            with (
                self.subTest(branch=branch),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)

                def git(*args):
                    return subprocess.check_output(
                        ["git", *args], stderr=subprocess.DEVNULL
                    )

                remote, checkout = root / "remote.git", root / "images"
                git("init", "--bare", "--initial-branch=" + branch, str(remote))
                git("clone", str(remote), str(checkout))
                git("-C", str(checkout), "config", "user.name", "Fixture")
                git(
                    "-C",
                    str(checkout),
                    "config",
                    "user.email",
                    "fixture@localhost.invalid",
                )
                git("-C", str(checkout), "config", "commit.gpgsign", "false")
                image = checkout / "public/images/2026/11/07/logo.png"
                image.parent.mkdir(parents=True)
                image.write_bytes(b"fixture image")
                subprocess.run(
                    ["sh", "-eu", "-c", first],
                    cwd=checkout,
                    check=True,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                before = git("--git-dir", str(remote), "rev-parse", "refs/heads/main")
                # Emulate the documented edit after the successful pull.
                edited = update.replace(
                    "# 修改 public/ 中的文件",
                    "printf updated > public/images/2026/11/07/logo.png",
                )
                subprocess.run(
                    ["sh", "-eu", "-c", edited],
                    cwd=checkout,
                    check=True,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                self.assertNotEqual(
                    before,
                    git("--git-dir", str(remote), "rev-parse", "refs/heads/main"),
                )
                self.assertEqual(
                    git(
                        "--git-dir",
                        str(remote),
                        "show",
                        "main:public/images/2026/11/07/logo.png",
                    ),
                    b"updated",
                )
