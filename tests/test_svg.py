"""Static SVG validation rejects active XML before publication writes."""

from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from open_assets import svg, __main__ as core


class SVG(unittest.TestCase):
    def check(self, body):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "image.svg"
            path.write_bytes(body)
            svg.validate(path)

    def test_static_shapes_inline_style_gradients_and_fragment_references(self):
        self.check(
            b"""<?xml version="1.0" encoding="UTF-8"?><svg xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="g"/></defs><style>rect{fill:url(#g)}</style><rect width="10" height="10"/><use href="#g"/></svg>"""
        )

    def test_active_and_external_inputs_are_rejected(self):
        for content in (
            b"<script>alert(1)</script>",
            b"<foreignObject/>",
            b'<g onload="alert(1)"/>',
            b'<use href="https://example.org/image.svg"/>',
            b'<image href="data:image/svg+xml;base64,PHN2Zy8+"/>',
            b'<set attributeName="onload" to="alert(1)"/>',
        ):
            with self.subTest(content=content), self.assertRaises(ValueError):
                self.check(
                    b'<svg xmlns="http://www.w3.org/2000/svg">' + content + b"</svg>"
                )

    def test_invalid_xml_dtd_and_encoding_are_rejected(self):
        for content in (
            b"<svg>",
            b"<html/>",
            b'<!DOCTYPE svg [<!ENTITY x "text">]><svg xmlns="http://www.w3.org/2000/svg">&x;</svg>',
            b'<?xml-stylesheet href="https://example.org/style.css"?><svg xmlns="http://www.w3.org/2000/svg"/>',
            "<svg/>".encode("utf-16"),
        ):
            with self.subTest(content=content), self.assertRaises(ValueError):
                self.check(content)

    def test_size_depth_and_element_budgets(self):
        with patch.object(svg, "MAX_BYTES", 10), self.assertRaises(ValueError):
            self.check(b'<svg xmlns="http://www.w3.org/2000/svg"/>')
        with self.assertRaises(ValueError):
            self.check(
                b'<svg xmlns="http://www.w3.org/2000/svg">'
                + b"<g>" * 129
                + b"</g>" * 129
                + b"</svg>"
            )
        with self.assertRaises(ValueError):
            self.check(
                b'<svg xmlns="http://www.w3.org/2000/svg">'
                + b"<g/>" * 20001
                + b"</svg>"
            )

    def test_serving_policy_keeps_scripts_sandboxed_without_allow_same_origin(self):
        with tempfile.TemporaryDirectory() as temporary:
            core.render(
                dict(repositories=[dict(key="images", mount_path="/")]), temporary
            )
            output = (Path(temporary) / "images.conf").read_text()
            self.assertIn(svg.CSP, output)
            self.assertIn("nosniff", output)
            self.assertNotIn("allow-scripts", svg.CSP)
            self.assertNotIn("allow-same-origin", svg.CSP)
