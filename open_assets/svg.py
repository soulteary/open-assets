"""Bounded validation of static SVG; bytes are retained for immutable releases.

The public gateway must also enforce CSP. This is not a general HTML sanitizer.
"""

import re
import xml.etree.ElementTree as ET

MAX_BYTES = 2 * 1024 * 1024
CSP = "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
NAMESPACE = "http://www.w3.org/2000/svg"


def validate(path):
    with path.open("rb") as source:
        raw = source.read(MAX_BYTES + 1)
    if len(raw) > MAX_BYTES:
        raise ValueError("SVG size limit exceeded")
    try:
        text = raw.decode("utf-8-sig")
    except UnicodeError:
        raise ValueError("SVG must use UTF-8") from None
    if re.search(r"<!\s*(?:DOCTYPE|ENTITY)\b|<\?(?!xml\s)", text, re.I):
        raise ValueError("SVG declarations and processing instructions are unsupported")
    parser = ET.XMLPullParser(events=("start", "end"))
    depth = count = 0
    try:
        for offset in range(0, len(text), 4096):
            parser.feed(text[offset : offset + 4096])
            for event, node in parser.read_events():
                if event == "end":
                    depth -= 1
                    node.clear()
                    continue
                depth += 1
                count += 1
                if depth > 128 or count > 20000:
                    raise ValueError("SVG complexity limit exceeded")
                if count == 1 and node.tag != "{" + NAMESPACE + "}svg":
                    raise ValueError("SVG root namespace required")
                if not node.tag.startswith("{" + NAMESPACE + "}"):
                    raise ValueError("non-SVG elements unsupported")
                if node.tag.rsplit("}", 1)[-1].lower() in {
                    "script",
                    "foreignobject",
                    "iframe",
                    "object",
                    "embed",
                    "audio",
                    "video",
                }:
                    raise ValueError("active SVG elements unsupported")
                for name, value in node.attrib.items():
                    name = name.rsplit("}", 1)[-1].lower()
                    if name.startswith("on"):
                        raise ValueError("SVG event handlers unsupported")
                    if name == "href" and not (
                        value.startswith("#")
                        or re.match(
                            r"^data:image/(?:png|jpeg|gif|webp|avif);base64,", value
                        )
                    ):
                        raise ValueError("external SVG references unsupported")
                if node.attrib.get("attributeName", "").lower().startswith("on"):
                    raise ValueError("SVG event animation unsupported")
        parser.close()
    except ET.ParseError:
        raise ValueError("invalid SVG XML") from None
