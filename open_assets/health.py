"""Small local container probes; diagnostics never include URLs or credentials."""

import json
import os
from pathlib import Path
import stat
import sys
import time
import urllib.request


def worker_healthy(record, now, max_job_seconds=1800):
    if not -5 <= now - record["updated_at"] <= 60:
        return False
    started = record.get("job_started_at")
    if started is not None:
        return 0 <= now - started <= max_job_seconds
    return -5 <= now - record["progress_at"] <= 120


def read_worker_heartbeat(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("heartbeat must be a regular file")
        data = stream.read(4097)
    if len(data) > 4096:
        raise ValueError("heartbeat exceeds limit")
    return json.loads(data)


def main():
    try:
        role = sys.argv[1]
        if role == "publisher":
            import yaml

            config = yaml.safe_load(Path(os.environ["OPEN_ASSETS_CONFIG"]).read_text())
            record = read_worker_heartbeat(
                Path(config["state_root"]) / "worker-heartbeat.json"
            )
            limit = int(os.environ.get("WORKER_MAX_JOB_SECONDS", "1800"))
            if limit < 60 or not worker_healthy(record, time.time(), limit):
                raise ValueError("worker heartbeat invalid")
        else:
            port = {"webhook": 9000, "info": 8082}[role]
            with urllib.request.urlopen(
                f"http://127.0.0.1:{port}/healthz", timeout=3
            ) as response:
                if response.status != 200:
                    raise ValueError("health endpoint failed")
        return 0
    except (OSError, ValueError, KeyError, TypeError, IndexError):
        print("Local health probe failed", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
