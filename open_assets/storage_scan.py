"""Low-priority, finite repository capacity sampling outside host/chart refresh."""

import argparse
import fcntl
import os
from pathlib import Path

from . import storage_info, telemetry


def main():
    parser = argparse.ArgumentParser(
        description="Bounded background local capacity scan"
    )
    parser.add_argument("--root", type=Path, default=Path("/app/open-assets"))
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run the host sampler as root")
    data = Path(telemetry.environment(args.root)["DATA_ROOT"])
    folder = data / "info"
    folder.mkdir(parents=True, exist_ok=True)
    with (folder / ".storage-scan.lock").open("a") as guard:
        fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
        storage_info.collect_storage(args.root, data, force=True, scan_seconds=80)
    print("Bounded local capacity sample updated")


if __name__ == "__main__":
    main()
