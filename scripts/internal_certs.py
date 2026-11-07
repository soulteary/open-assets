"""Generate a new ten-year internal TLS tree using a verified Certs Maker release.

Generation never replaces a running deployment. Installing a candidate requires
the documented backup, client trust update and coordinated service restart.
"""

import argparse
import json
import os
import shutil
import ssl
import subprocess
import time
from pathlib import Path

from certs_maker import ensure_binary

DAYS = 3650


def run(*args, input=None, cwd=None):
    return subprocess.run(
        args, input=input, cwd=cwd, capture_output=True, check=True, timeout=120
    ).stdout


def generate(output, domains, binary):
    output = Path(output).resolve()
    if output.exists() and any(output.iterdir()):
        raise ValueError(
            "output must be absent or empty; never generate over live certificates"
        )
    output.mkdir(parents=True, mode=0o700, exist_ok=True)
    output.chmod(0o700)
    os.umask(0o077)
    roots = []
    for role, names, uid in [
        ("storage", "storage.internal", 20002),
        ("console", domains["storage"], 20004),
    ]:
        work = output / "issuers" / role
        work.mkdir(parents=True, mode=0o700)
        run(
            str(binary),
            "--CERT_DNS=" + names,
            "--FOR_FIREFOX=ON",
            f"--EXPIRE_DAYS={DAYS}",
            "--CUSTOM_FILE_NAME=server",
            "--CERT_O=Open Assets",
            "--DIR=ssl",
            cwd=work,
        )
        generated = work / "ssl"
        root = generated / "server.rootCA.pem"
        cert, key = generated / "server.pem.crt", generated / "server.pem.key"
        # Prove chain, both service identities, key match, and requested validity.
        for hostname in names.split(","):
            run(
                "openssl",
                "verify",
                "-CAfile",
                str(root),
                "-verify_hostname",
                hostname,
                str(cert),
            )
        cert_key = run("openssl", "x509", "-in", str(cert), "-noout", "-pubkey")
        private_key = run("openssl", "pkey", "-in", str(key), "-pubout")
        if cert_key != private_key:
            raise ValueError("certificate/key mismatch")
        for path in (root, cert):
            dates = dict(
                line.split("=", 1)
                for line in run("openssl", "x509", "-in", str(path), "-noout", "-dates")
                .decode()
                .splitlines()
            )
            validity = ssl.cert_time_to_seconds(
                dates["notAfter"]
            ) - ssl.cert_time_to_seconds(dates["notBefore"])
            if (
                validity != DAYS * 86400
                or ssl.cert_time_to_seconds(dates["notAfter"])
                < time.time() + (DAYS - 1) * 86400
            ):
                raise ValueError("unexpected certificate validity")
        destination = output / role
        destination.mkdir(mode=0o750)
        cert_name, key_name = (
            ("public.crt", "private.key")
            if role == "storage"
            else ("console.crt", "console.key")
        )
        shutil.copyfile(cert, destination / cert_name)
        shutil.copyfile(key, destination / key_name)
        shutil.copyfile(root, work / "root.crt")
        shutil.copyfile(generated / "server.rootCA.key", work / "root.key")
        for path in work.rglob("*"):
            path.chmod(0o700 if path.is_dir() else 0o600)
        roots.append(root.read_bytes())
        os.chown(destination, uid, uid)
        for path in destination.iterdir():
            os.chown(path, uid, uid)
            path.chmod(0o640 if role == "storage" else 0o600)
    bundle = b"\n".join(roots)
    (output / "ca.crt").write_bytes(bundle)
    for role, uid in [("storage", 20002), ("console", 20004)]:
        path = output / role / "ca.crt"
        path.write_bytes(bundle)
        os.chown(path, uid, uid)
        path.chmod(0o640 if role == "storage" else 0o600)
    (output / "storage/CAs").mkdir(mode=0o750)
    os.chown(output / "storage/CAs", 20002, 20002)
    output.chmod(0o711)


def main():
    parser = argparse.ArgumentParser(
        description="Generate a new, verified 3650-day internal TLS candidate"
    )
    parser.add_argument("--domains", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--root", type=Path, default=Path("/app/open-assets"))
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run on the trusted deployment host as root")
    settings = dict(
        line.split("=", 1)
        for line in (args.root / ".env").read_text().splitlines()
        if "=" in line and not line.startswith("#")
    )
    generate(
        args.output,
        json.loads(args.domains.read_text()),
        ensure_binary(args.root, settings, args.archive),
    )
    print("Two verified server certificates and issuing CAs generated (3650 days)")


if __name__ == "__main__":
    main()
