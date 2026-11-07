"""Install an exact official Certs Maker archive without unpacking arbitrary paths."""

import hashlib
import io
import json
import os
from pathlib import Path
import platform
import tarfile
import urllib.request


def ensure_binary(app, settings, archive_path=None):
    if platform.system() != "Linux":
        raise ValueError("the deployment recipe supports Linux only")
    architecture = {"x86_64": "amd64", "aarch64": "arm64"}.get(platform.machine())
    if not architecture:
        raise ValueError("unsupported Certs Maker platform")
    version = settings["CERTS_MAKER_VERSION"]
    expected = settings["CERTS_MAKER_SHA256_" + architecture.upper()]
    tools = Path(app) / "private/tools"
    tools.mkdir(mode=0o700, exist_ok=True)
    binary, receipt = tools / "certs-maker", tools / "certs-maker.json"
    if binary.exists() and receipt.exists():
        record = json.loads(receipt.read_text())
        if (
            record.get("version") == version
            and record.get("archive_sha256") == expected
        ):
            if (
                hashlib.sha256(binary.read_bytes()).hexdigest()
                != record["binary_sha256"]
            ):
                raise ValueError("installed Certs Maker digest mismatch")
            return binary
    if archive_path:
        archive = Path(archive_path).read_bytes()
    else:
        name = f"certs-maker_{version}_linux_{architecture}.tar.gz"
        url = f"https://github.com/soulteary/certs-maker/releases/download/v{version}/{name}"
        with urllib.request.urlopen(url, timeout=60) as response:
            archive = response.read(32 * 1024**2 + 1)
    if len(archive) > 32 * 1024**2 or hashlib.sha256(archive).hexdigest() != expected:
        raise ValueError("official Certs Maker archive checksum mismatch")
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as source:
        member = next(
            (m for m in source if m.name == "certs-maker" and m.isfile()), None
        )
        if member is None:
            raise ValueError("archive does not contain the expected regular binary")
        if member.size > 64 * 1024**2:
            raise ValueError("unexpected Certs Maker binary size")
        data = source.extractfile(member).read()
    temporary = tools / ".certs-maker.new"
    temporary.write_bytes(data)
    temporary.chmod(0o700)
    os.replace(temporary, binary)
    receipt.write_text(
        json.dumps(
            dict(
                version=version,
                archive_sha256=expected,
                binary_sha256=hashlib.sha256(data).hexdigest(),
            )
        )
    )
    receipt.chmod(0o600)
    return binary
