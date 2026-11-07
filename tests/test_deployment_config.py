import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "deployment_config",
    Path(__file__).resolve().parents[1] / "scripts/deployment_config.py",
)
config = importlib.util.module_from_spec(spec)
spec.loader.exec_module(config)


class NodeNaming(unittest.TestCase):
    def test_default_and_custom_node(self):
        for node in (config.DEFAULT_NODE_NAME, "beijing-1", "a", "a" * 48):
            domains = config.build_domains(node, "example.org")
            self.assertEqual(domains["img"], f"img-{node}.example.org")
            self.assertEqual(domains["storage"], f"storage-{node}.console.example.org")
            self.assertEqual(len(domains), 7)

    def test_reject_invalid_dns_before_rendering(self):
        for node in ("", "A", "-node", "node-", "x.y", "a/b", "${OTHER}", "a" * 49):
            with self.subTest(node=node), self.assertRaises(ValueError):
                config.build_domains(node, "example.org")
        for base in (
            "",
            "localhost",
            "https://example.org",
            "example.org/",
            "a..org",
            "*.org",
        ):
            with self.subTest(base=base), self.assertRaises(ValueError):
                config.build_domains("node-name", base)

    def test_env_render_and_read_only_check_detect_drift(self):
        template = Path(".env.example").read_text()
        rendered = config.render_env(template, "node-2", "example.org")
        self.assertIn("NODE_NAME=node-2\n", rendered)
        self.assertIn("STORAGE_DOMAIN=storage-node-2.console.example.org\n", rendered)
        self.assertEqual(
            [s for s in template.splitlines() if "_IMAGE=" in s],
            [s for s in rendered.splitlines() if "_IMAGE=" in s],
        )
        with tempfile.TemporaryDirectory() as tmp:
            env, domains = Path(tmp) / "env", Path(tmp) / "domains.json"
            env.write_text(rendered)
            domains.write_text(
                json.dumps(config.build_domains("node-2", "example.org"))
            )
            before = (env.read_bytes(), domains.read_bytes())
            config.check_naming(env, domains)
            self.assertEqual(before, (env.read_bytes(), domains.read_bytes()))
            env.write_text(rendered.replace("NODE_NAME=node-2", "NODE_NAME=node-3"))
            with self.assertRaises(ValueError):
                config.check_naming(env, domains)
            self.assertEqual(domains.read_bytes(), before[1])

    def test_missing_template_field_rejected(self):
        with self.assertRaises(ValueError):
            config.render_env("DATA_ROOT=/data\n", "node-name", "example.org")
