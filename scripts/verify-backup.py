"""Check a snapshot checksum and extract only into a new isolated drill directory.

This validates archive readability and safe extraction, not application recovery.
It never starts services or writes to the archived absolute deployment paths.
"""

import argparse
import hashlib
from pathlib import Path
import re
import tarfile


def extract(archive, destination):
    archive, destination = Path(archive), Path(destination)
    checksum = Path(str(archive) + ".sha256").read_text().split()[0]
    if not re.fullmatch(r"[a-f0-9]{64}", checksum):
        raise ValueError("invalid snapshot checksum")
    with archive.open("rb") as stream:
        actual = hashlib.file_digest(stream, "sha256").hexdigest()
    if actual != checksum:
        raise ValueError("snapshot checksum mismatch")
    # Exclusive creation prevents accidentally restoring over an existing tree.
    destination.mkdir(mode=0o700)
    with tarfile.open(archive, "r:gz") as source:
        source.extractall(destination, filter="data")
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("destination", type=Path)
    options = parser.parse_args()
    extract(options.archive, options.destination)
    print(
        "Checksum and isolated extraction passed; complete the application restore drill."
    )


if __name__ == "__main__":
    main()
