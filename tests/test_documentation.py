"""Keep active documentation links and the supported operator entry consistent."""

from pathlib import Path
import re
import unittest


class Documentation(unittest.TestCase):
    def test_local_markdown_links_resolve(self):
        files = [
            Path("README.md"),
            Path("CONTRIBUTING.md"),
            Path(".github/PULL_REQUEST_TEMPLATE.md"),
            *Path("docs").rglob("*.md"),
            Path("tests/e2e/README.md"),
        ]
        for path in files:
            for target in re.findall(r"\[[^\]]*\]\(([^\s)]+)\)", path.read_text()):
                if target.startswith(("https:", "http:", "mailto:", "#")):
                    continue
                with self.subTest(path=path, target=target):
                    destination, _, anchor = target.partition("#")
                    destination = path.parent / destination
                    self.assertTrue(destination.exists())
                    if anchor and destination.suffix == ".md":
                        slugs = set()
                        for heading in re.findall(
                            r"^#{1,6}\s+(.+)$", destination.read_text(), re.M
                        ):
                            base = re.sub(
                                r"[^\w\- ]", "", heading.strip().lower()
                            ).replace(" ", "-")
                            slug, number = base, 0
                            while slug in slugs:
                                number += 1
                                slug = base + "-" + str(number)
                            slugs.add(slug)
                        self.assertIn(anchor, slugs)

    def test_daily_commands_use_host_cli_and_no_test_environment_details(self):
        for path in [Path("README.md"), *Path("docs").glob("*.md")]:
            text = path.read_text()
            with self.subTest(path=path):
                self.assertNotRegex(
                    text,
                    r"python -m open_assets\s+(?:\\\n\s*)?(?:status|publish|retry|rollback-images|restore-objects|check-restore|close-restore|restore-audit-)\b",
                )
                self.assertNotIn("服务器验收用的测试密钥", text)
                self.assertNotIn("用于验收的 `content/project-a`", text)
                self.assertNotIn("Tool/environment observations:", text)
